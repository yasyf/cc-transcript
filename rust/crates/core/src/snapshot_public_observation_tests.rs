fn public_observation_store() -> NativeStore {
    NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap()
}

fn public_observation_resume(store: &NativeStore, cursor: &str, owner: &Value) -> Value {
    store.request(&json!({"schema":SCHEMA,"id":"public-observation-resume","operation":"resume","cursor":cursor}), owner, &Cancellation::default())
}

fn public_observation_collect(store: &NativeStore, mut response: Value, owner: &Value) -> (Value, Vec<Value>, u64) {
    let mut rows = Vec::new();
    let mut entries = 0;
    for _ in 0..4096 {
        assert!(matches!(response["status"].as_str(), Some("ok" | "incomplete")), "{response:?}");
        entries += response["usage"]["discovery_entries_examined"].as_u64().unwrap();
        rows.extend(response["data"]["sessions"].as_array().unwrap().iter().cloned());
        let Some(cursor) = response["cursor"].as_str() else { return (response, rows, entries); };
        response = public_observation_resume(store, cursor, owner);
    }
    panic!("public observation cursor did not settle");
}

#[test]
fn public_catalog_observation_first_result_is_frozen_but_the_next_query_is_fresh() {
    for directory in [false, true] {
        let source = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        let path = source.directory.join("unrelated.txt");
        std::fs::write(&path, "").unwrap();
        let changed = path.clone();
        let called = Arc::new(AtomicUsize::new(0));
        let callback = Arc::clone(&called);
        let store = store();
        *store.location_metadata_hook.lock().unwrap() = Some(("prefilter", Arc::new(move || {
            callback.fetch_add(1, Ordering::Relaxed);
            std::fs::remove_file(&changed).unwrap();
            if directory {
                std::fs::create_dir(&changed).unwrap();
                std::fs::write(changed.join("wanted.jsonl"), "").unwrap();
            } else {
                std::os::unix::fs::symlink("unrelated.txt", &changed).unwrap();
            }
        })));
        let owner = context("public-observation-first");
        let request = public_catalog_request(&source, &["wanted"]);
        let (first, rows, entries) = public_observation_collect(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
        assert_eq!(called.load(Ordering::Relaxed), 1);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"].as_str(), Some("missing"));
        assert_eq!(entries, 1);
        assert_eq!(store.location_entry_canonicalizations.load(Ordering::Relaxed), 0);
        let frozen = first.clone();
        let next = store.request(&request, &owner, &Cancellation::default());
        if directory {
            let (next, rows, entries) = public_observation_collect(&store, next, &owner);
            assert_eq!(next["status"].as_str(), Some("ok"), "{next:?}");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["status"].as_str(), Some("ok"));
            assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()), path.join("wanted.jsonl"));
            assert_eq!(entries, 2, "the next query must examine the new directory and child");
        } else {
            assert_eq!(next["status"].as_str(), Some("changed"), "{next:?}");
            assert!(next["data"].is_null());
            assert!(next["cursor"].is_null());
        }
        assert_eq!(first, frozen);
        assert_eq!(rows[0]["status"].as_str(), Some("missing"));
        store.assert_conserved();
    }
}

#[test]
fn public_catalog_observation_foreign_prefetch_spends_the_remaining_private_name_budget() {
    let source = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..128 {
        std::fs::write(source.directory.join(format!("ordinary-{index:03}.txt")), "").unwrap();
    }
    let store = public_observation_store();
    let owner = context("public-observation-budget-a");
    let foreign = context("public-observation-budget-b");
    let mut request = public_catalog_request(&source, &["absent"]);
    request["limits"].insert("max_discovery_entries", json!(2));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("one-name observation cursor").to_owned();
    let mut prefetch = request.clone();
    prefetch["limits"].insert("max_discovery_entries", json!(64));
    let (bounded, rows, entries) = public_observation_collect(&store, store.request(&prefetch, &foreign, &Cancellation::default()), &foreign);
    assert_eq!(bounded["status"].as_str(), Some("incomplete"), "{bounded:?}");
    assert!(bounded["cursor"].is_null());
    assert_eq!(entries, 64);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"].as_str(), Some("incomplete"));
    let resumed = public_observation_resume(&store, &cursor, &owner);
    assert_eq!(resumed["status"].as_str(), Some("incomplete"), "{resumed:?}");
    assert!(resumed["cursor"].is_null());
    assert_eq!(resumed["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert_eq!(resumed["data"]["sessions"][0]["status"].as_str(), Some("incomplete"));
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_warm_negative_at_initial_zero_discovery_is_still_incomplete() {
    let source = Source::new("");
    let store = store();
    let owner = context("public-observation-zero-negative");
    let mut request = public_catalog_request(&source, &["absent"]);
    let (cold, cold_entries) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(cold[0]["status"].as_str(), Some("missing"));
    assert_eq!(cold_entries, 1);
    let (warm, warm_entries) = public_catalog_finish(&store, &request, &owner);
    assert_eq!(warm[0]["status"].as_str(), Some("missing"));
    assert_eq!(warm_entries, 0);
    request["limits"].insert("max_discovery_entries", json!(0));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
    assert_eq!(response["reason"].as_str(), Some("bounded location incomplete"));
    assert!(response["cursor"].is_null());
    assert_eq!(response["data"]["kind"].as_str(), Some("located"));
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(0));
    let rows = response["data"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session_id"].as_str(), Some("absent"));
    assert_eq!(rows[0]["status"].as_str(), Some("incomplete"));
    assert!(rows[0]["path"].is_null());
    assert!(rows[0]["revision"].is_null());
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_old_alias_names_use_current_logical_child_metadata_after_foreign_rebuild() {
    let source = Source::new("");
    let old = source.directory.join("old");
    let new = source.directory.join("new");
    let alias = source.directory.join("alias");
    std::fs::create_dir(&old).unwrap();
    std::fs::create_dir(&new).unwrap();
    for index in 0..4 { std::fs::write(old.join(format!("ordinary-{index}.txt")), "").unwrap(); }
    let names: Vec<_> = std::fs::read_dir(&old).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(names.len(), 4);
    std::fs::write(new.join(&names[0]), "").unwrap();
    for name in &names[1..] {
        std::fs::create_dir(new.join(name)).unwrap();
        std::fs::write(new.join(name).join("wanted.jsonl"), "").unwrap();
    }
    std::fs::write(new.join("foreign-only.jsonl"), "").unwrap();
    std::os::unix::fs::symlink(&old, &alias).unwrap();
    let store = public_observation_store();
    let owner = context("public-observation-alias-a");
    let foreign = context("public-observation-alias-b");
    let mut request = public_catalog_request(&source, &["wanted", "foreign-only"]);
    request.insert("roots", json!([alias]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("old physical name iterator").to_owned();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&new, &alias).unwrap();
    let mut rebuilt = request.clone();
    rebuilt.insert("session_ids", json!(["not-present"]));
    let (_, rebuilt_rows, rebuilt_entries) = public_observation_collect(&store, store.request(&rebuilt, &foreign, &Cancellation::default()), &foreign);
    assert_eq!(rebuilt_rows[0]["status"].as_str(), Some("missing"));
    assert_eq!(rebuilt_entries, 8);
    let (last, rows, entries) = public_observation_collect(&store, public_observation_resume(&store, &cursor, &owner), &owner);
    assert_eq!(last["status"].as_str(), Some("ok"), "{last:?}");
    assert_eq!(entries, 6, "remaining old names plus their current nested children");
    assert_eq!(rows.len(), 2);
    let wanted = rows.iter().find(|row| row["session_id"].as_str() == Some("wanted")).unwrap();
    assert_eq!(wanted["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(wanted["path"].as_str().unwrap()), alias.join(&names[1]).join("wanted.jsonl"));
    let absent = rows.iter().find(|row| row["session_id"].as_str() == Some("foreign-only")).unwrap();
    assert_eq!(absent["status"].as_str(), Some("missing"), "new-generation names must not replace the old iterator");
    let fresh = public_catalog_finish(&store, &request, &foreign).0;
    assert!(fresh.iter().all(|row| row["status"].as_str() == Some("ok")));
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&old, &alias).unwrap();
    let restored = public_catalog_finish(&store, &request, &foreign).0;
    assert!(restored.iter().all(|row| row["status"].as_str() == Some("missing")), "private logical facts must not poison the unchanged old physical directory");
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_same_physical_directory_is_not_revisited_in_a_new_generation() {
    let source = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..4 { std::fs::write(source.directory.join(format!("ordinary-{index}.txt")), "").unwrap(); }
    let names: Vec<_> = std::fs::read_dir(&source.directory).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    let alias_holder = Source::new("");
    let alias = alias_holder.directory.join("alias");
    std::os::unix::fs::symlink(&source.directory, &alias).unwrap();
    let store = public_observation_store();
    let owner = context("public-observation-identity-a");
    let foreign = context("public-observation-identity-b");
    let mut request = public_catalog_request(&source, &["wanted"]);
    request.insert("roots", json!([alias, source.directory]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let cursor = first["cursor"].as_str().expect("first physical directory cursor").to_owned();
    let consumed = source.directory.join(&names[0]);
    std::fs::remove_file(&consumed).unwrap();
    std::fs::create_dir(&consumed).unwrap();
    std::fs::write(consumed.join("wanted.jsonl"), "").unwrap();
    let mut rebuilt = request.clone();
    rebuilt.insert("session_ids", json!(["not-present"]));
    let (_, rows, foreign_entries) = public_observation_collect(&store, store.request(&rebuilt, &foreign, &Cancellation::default()), &foreign);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"));
    assert_eq!(foreign_entries, 5);
    let (last, rows, entries) = public_observation_collect(&store, public_observation_resume(&store, &cursor, &owner), &owner);
    assert_eq!(last["status"].as_str(), Some("ok"), "{last:?}");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"), "neither a new generation nor a second alias revisits the consumed physical directory");
    assert_eq!(entries, 3);
    let fresh = public_catalog_finish(&store, &request, &foreign).0;
    assert_eq!(fresh[0]["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(fresh[0]["path"].as_str().unwrap()), consumed.join("wanted.jsonl"));
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_retargeted_unconsumed_alias_name_uses_current_authority() {
    let source = Source::new("");
    let outside = Source::new("");
    let old = source.directory.join("old");
    let new = source.directory.join("new");
    let alias = source.directory.join("alias");
    std::fs::create_dir(&old).unwrap();
    std::fs::create_dir(&new).unwrap();
    for index in 0..4 { std::fs::write(old.join(format!("ordinary-{index}.txt")), "").unwrap(); }
    let names: Vec<_> = std::fs::read_dir(&old).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    for name in &names { std::fs::write(new.join(name), "").unwrap(); }
    std::fs::remove_file(new.join(&names[1])).unwrap();
    std::os::unix::fs::symlink(&outside.directory, new.join(&names[1])).unwrap();
    std::os::unix::fs::symlink(&old, &alias).unwrap();
    let store = public_observation_store();
    let owner = context("public-observation-raw-authority");
    let mut request = public_catalog_request(&source, &["wanted"]);
    request.insert("roots", json!([alias]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let cursor = first["cursor"].as_str().expect("old raw-route frame").to_owned();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&new, &alias).unwrap();
    let foreign = context("public-observation-raw-authority-foreign");
    let (rows, _) = public_catalog_finish(&store, &request, &foreign);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"));
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[new],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let before = store.authority_paths_materialized.load(Ordering::Relaxed);
    let response = public_observation_resume(&store, &cursor, &narrowed);
    assert_eq!(response["status"].as_str(), Some("permission_denied"), "{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert_eq!(store.authority_paths_materialized.load(Ordering::Relaxed) - before, 1);
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_open_parent_survives_narrowing_but_a_new_caller_cannot_borrow_it() {
    let source = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let nested = source.directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let wanted = nested.join("wanted.jsonl");
    std::fs::write(&wanted, "").unwrap();
    let store = public_observation_store();
    let owner = context("public-observation-parent");
    let request = public_catalog_request(&source, &["wanted"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("already-open parent cursor").to_owned();
    let foreign = context("public-observation-parent-foreign");
    let mut prefetch = request.clone();
    prefetch.insert("session_ids", json!(["absent"]));
    let (rows, _) = public_catalog_finish(&store, &prefetch, &foreign);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"));
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[nested],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let (last, rows, entries) = public_observation_collect(&store, public_observation_resume(&store, &cursor, &narrowed), &narrowed);
    assert_eq!(last["status"].as_str(), Some("ok"), "{last:?}");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()), wanted);
    assert_eq!(entries, 1);
    narrowed.insert("claimant", json!("public-observation-parent-new"));
    let before = store.authority_paths_materialized.load(Ordering::Relaxed);
    let denied = store.request(&request, &narrowed, &Cancellation::default());
    assert_eq!(denied["status"].as_str(), Some("permission_denied"), "{denied:?}");
    assert!(denied["data"].is_null());
    assert!(denied["cursor"].is_null());
    assert_eq!(store.authority_paths_materialized.load(Ordering::Relaxed) - before, 1);
    store.assert_conserved();
}

#[test]
fn public_catalog_observation_queued_rows_keep_current_path_and_authority_guards() {
    for denied in [false, true] {
        let source = Source::new("");
        let outside = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        let mut directory = source.directory.clone();
        for name in ["d", "e", "f"] {
            directory = directory.join(name.repeat(160));
            std::fs::create_dir(&directory).unwrap();
        }
        let ids: Vec<_> = (0..1024).map(|index| format!("{index:04}-{}", "x".repeat(190))).collect();
        for id in &ids { std::fs::write(directory.join(format!("{id}.jsonl")), "").unwrap(); }
        let wanted: Vec<_> = ids.iter().map(String::as_str).collect();
        let store = NativeStore::new(&json!({"max_events_per_step":128,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("public-observation-queued-guard");
        let mut request = public_catalog_request(&source, &wanted);
        request["limits"].insert("max_output_bytes", json!(8 * 1024 * 1024));
        let (primed, _) = public_catalog_finish(&store, &request, &owner);
        assert_eq!(primed.len(), ids.len());
        assert!(primed.iter().all(|row| row["status"].as_str() == Some("ok")));
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(0));
        let emitted = first["data"]["sessions"].as_array().unwrap();
        assert!(!emitted.is_empty() && emitted.len() < ids.len());
        let pending_id = &ids[emitted.len()];
        let pending_path = directory.join(format!("{pending_id}.jsonl"));
        let cursor = first["cursor"].as_str().expect("queued successful rows").to_owned();
        std::fs::remove_file(&pending_path).unwrap();
        if denied {
            std::os::unix::fs::symlink(&outside.path, &pending_path).unwrap();
            let mut narrowed = owner.clone();
            narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
            let before = store.authority_paths_materialized.load(Ordering::Relaxed);
            let response = public_observation_resume(&store, &cursor, &narrowed);
            assert_eq!(response["status"].as_str(), Some("permission_denied"), "{response:?}");
            assert!(response["data"].is_null());
            assert!(response["cursor"].is_null());
            assert_eq!(store.authority_paths_materialized.load(Ordering::Relaxed) - before, 1);
        } else {
            let (last, rows, entries) = public_observation_collect(&store, public_observation_resume(&store, &cursor, &owner), &owner);
            assert_eq!(last["status"].as_str(), Some("ok"), "{last:?}");
            assert_eq!(entries, 0);
            assert!(rows.iter().all(|row| row["session_id"].as_str() != Some(pending_id.as_str())), "a disappeared queued path must not be published as successful");
            assert_eq!(emitted.len() + rows.len(), ids.len() - 1);
        }
        store.assert_conserved();
    }
}

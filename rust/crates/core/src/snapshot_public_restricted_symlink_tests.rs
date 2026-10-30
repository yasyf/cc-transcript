#[test]
fn public_catalog_restricted_symlink_unchanged_negative_reuses_names() {
    for ordinary_count in [32usize, 64] {
        let source = Source::new("");
        let target = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        for index in 0..ordinary_count {
            std::fs::write(source.directory.join(format!("ordinary-{index:03}.txt")), "").unwrap();
        }
        let link = source.directory.join("allowed-link.jsonl");
        std::os::unix::fs::symlink(&target.path, &link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target.path);
        let resolved = std::fs::canonicalize(&target.path).unwrap();
        assert_eq!(std::fs::canonicalize(&link).unwrap(), resolved);
        let parent_stamp = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
        let link_stamp = SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap());
        let target_stamp = SourceStamp::of(&std::fs::metadata(&target.path).unwrap());
        let names: HashSet<_> = std::fs::read_dir(&source.directory).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(names.len(), ordinary_count + 1);
        let store = NativeStore::new(&json!({"max_events_per_step":128,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let mut owner = context("public-restricted-symlink-negative");
        owner.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory,target.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
        let request = public_catalog_request(&source, &["absent"]);
        let (cold, cold_entries) = public_catalog_finish(&store, &request, &owner);
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0]["session_id"].as_str(), Some("absent"));
        assert_eq!(cold[0]["status"].as_str(), Some("missing"));
        assert_eq!(cold_entries, (ordinary_count + 1) as u64);
        let slot = {
            let state = store.lock_state();
            assert_eq!(state.catalogs.len(), 1);
            state.catalogs.values().next().cloned().unwrap()
        };
        let cold_appends = {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("cold public query must settle"); };
            catalog.physical_append_work
        };
        assert_eq!(cold_appends, (ordinary_count + 1) as u64);
        let (warm, warm_entries) = public_catalog_finish(&store, &request, &owner);
        let warm_appends = {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("warm public query must settle"); };
            catalog.physical_append_work
        };
        eprintln!("restricted_symlink_ordinary={ordinary_count} cold_entries={cold_entries} warm_entries={warm_entries} physical_append_delta={}", warm_appends - cold_appends);
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0]["status"].as_str(), Some("missing"));
        assert_eq!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()), parent_stamp);
        assert_eq!(SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap()), link_stamp);
        assert_eq!(SourceStamp::of(&std::fs::metadata(&target.path).unwrap()), target_stamp);
        assert_eq!(std::fs::read_link(&link).unwrap(), target.path);
        assert_eq!(std::fs::canonicalize(&link).unwrap(), resolved);
        assert_eq!(std::fs::read_dir(&source.directory).unwrap().map(|entry| entry.unwrap().file_name()).collect::<HashSet<_>>(), names);
        assert_eq!(warm_entries, 0, "an unchanged allowed restricted symlink must not rediscover its containing directory");
        assert_eq!(warm_appends, cold_appends, "target authorization may revalidate without appending unchanged names again");
        let selected = public_catalog_request(&source, &["allowed-link"]);
        let (allowed, _) = public_catalog_finish(&store, &selected, &owner);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0]["status"].as_str(), Some("ok"));
        assert_eq!(Path::new(allowed[0]["path"].as_str().unwrap()), link);
        let mut narrowed = owner.clone();
        narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
        let denied = store.request(&request, &narrowed, &Cancellation::default());
        assert_eq!(denied["status"].as_str(), Some("permission_denied"), "{denied:?}");
        assert!(denied["data"].is_null());
        assert!(denied["cursor"].is_null());
        store.assert_conserved();
    }
}

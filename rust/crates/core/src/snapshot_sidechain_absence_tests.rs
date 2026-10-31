fn sidechain_close_catalog(store: &NativeStore, request: &Value, owner: &Value) -> CatalogCertificate {
    let missing = json!(["sidechain-coverage-not-a-registered-source"]);
    let mut remaining = limits(request).unwrap();
    match store.catalog_access(CatalogInput {
        ids: missing.as_array().unwrap(), roots: request["roots"].as_array().unwrap(),
    }, owner, &Cancellation::default(), &mut remaining, &mut [0; 20], CatalogMode::Locate).unwrap() {
        CatalogResolution::Ready { certificate, .. } => certificate.unwrap(),
        _ => panic!("closed catalog lookup"),
    }
}

fn sidechain_batch_fixture(count: usize) -> (Source, NativeStore, Value, Value, WarmMembership) {
    let source = Source::new(&format!("{}\n", user("root")));
    let ids: Vec<_> = (0..count).map(|index| format!("batch-{index:04}")).collect();
    for id in &ids {
        std::fs::write(source.directory.join(format!("{id}.jsonl")), format!("{}\n", user(id))).unwrap();
    }
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":128,
        "max_entry_bytes":8192,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_leases":16,"reserved_hook_leases":1})).unwrap();
    let owner = context("sidechain-batch");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    assert_eq!(root["status"].as_str(), Some("ok"), "{root:?}");
    let mut request = catalog_missing_request(&source, &root, &ids);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    request["limits"].insert("max_sources", json!(8192));
    sidechain_close_catalog(&store, &request, &owner);
    let membership = catalog_cached_membership(&store, &request, &owner, true);
    (source, store, owner, request, membership)
}

#[test]
fn registered_catalog_sidechain_batch_removes_only_certified_absent_probes() {
    for count in [33, 129, 513, 5523] {
        let (_source, store, owner, request, membership) = sidechain_batch_fixture(count);
        assert_eq!(membership.members.len(), count);
        assert_eq!(membership.sidechain_dirs.len(), count);
        let cancel = Cancellation::default();
        let deadline = now_ms() + 30_000;
        let before = store.sidechain_path_probes.load(Ordering::Relaxed);
        let checks = store.membership_metadata_checks.load(Ordering::Relaxed);
        assert!(store.validate_warm_membership(&membership.members, &membership.sidechain_dirs,
            &owner, &cancel, deadline, None).unwrap());
        let old_probes = store.sidechain_path_probes.load(Ordering::Relaxed) - before;
        let old_checks = store.membership_metadata_checks.load(Ordering::Relaxed) - checks;
        let names = store.sidechain_batch_name_checks.load(Ordering::Relaxed);
        let proof = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
            &owner, &cancel, deadline).unwrap().expect("closed missing-prefix proof");
        let name_checks = store.sidechain_batch_name_checks.load(Ordering::Relaxed) - names;
        let before = store.sidechain_path_probes.load(Ordering::Relaxed);
        let checks = store.membership_metadata_checks.load(Ordering::Relaxed);
        assert!(store.validate_warm_membership(&membership.members, &membership.sidechain_dirs,
            &owner, &cancel, deadline, Some(&proof)).unwrap());
        let new_probes = store.sidechain_path_probes.load(Ordering::Relaxed) - before;
        let new_checks = store.membership_metadata_checks.load(Ordering::Relaxed) - checks;
        eprintln!("sidechain_batch sources={count} old_probes={old_probes} new_probes={new_probes} old_checks={old_checks} new_checks={new_checks} name_checks={name_checks}");
        assert_eq!(old_probes, count);
        assert_eq!(new_probes, 0);
        assert_eq!(old_checks, 2 * count);
        assert_eq!(new_checks, count);
        assert_eq!(name_checks, count + 1);
        drop(proof);
        let before = store.sidechain_path_probes.load(Ordering::Relaxed);
        let mut current_request = request.clone();
        current_request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let fresh = catalog_cached_membership(&store, &current_request, &owner, true);
        assert_eq!(store.sidechain_path_probes.load(Ordering::Relaxed) - before, 0);
        assert!(Arc::ptr_eq(&fresh.members, &membership.members));
        assert_eq!(fresh.revision, membership.revision);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_sidechain_batch_matching_ordinary_name_preserves_not_directory() {
    let (_source, store, owner, request, membership) = sidechain_batch_fixture(2);
    let prefix = membership.sidechain_dirs[0].0.parent().unwrap();
    std::fs::write(prefix, "ordinary file").unwrap();
    let certificate = sidechain_close_catalog(&store, &request, &owner);
    let proof = store.sidechain_absences(Some(certificate), &membership.sidechain_dirs,
        &owner, &Cancellation::default(), now_ms() + 30_000).unwrap().unwrap();
    assert!(!proof.contains(0));
    assert!(proof.contains(1));
    let old = store.validate_sidechain_dirs(&membership.sidechain_dirs, &owner,
        &Cancellation::default(), now_ms() + 30_000, None).unwrap_err();
    let new = store.validate_sidechain_dirs(&membership.sidechain_dirs, &owner,
        &Cancellation::default(), now_ms() + 30_000, Some(&proof)).unwrap_err();
    assert_eq!(old.status, Status::Changed);
    assert_eq!(old.status, new.status);
    assert_eq!(old.reason, new.reason);
    drop(proof);
    store.assert_conserved();
}

#[test]
fn registered_catalog_sidechain_batch_matching_directory_and_conditional_parent_use_existing_probes() {
    for symlink in [false, true] {
        let (source, store, owner, request, membership) = sidechain_batch_fixture(2);
        let prefix = membership.sidechain_dirs[0].0.parent().unwrap();
        if symlink {
            std::os::unix::fs::symlink(&source.path, prefix).unwrap();
        } else {
            std::fs::create_dir_all(&membership.sidechain_dirs[0].0).unwrap();
        }
        let certificate = sidechain_close_catalog(&store, &request, &owner);
        let proof = store.sidechain_absences(Some(certificate), &membership.sidechain_dirs,
            &owner, &Cancellation::default(), now_ms() + 30_000).unwrap();
        assert!(proof.as_ref().is_none_or(|proof| !proof.contains(0)));
        if symlink { assert!(proof.is_none()); }
        let old = store.validate_sidechain_dirs(&membership.sidechain_dirs, &owner,
            &Cancellation::default(), now_ms() + 30_000, None);
        let new = store.validate_sidechain_dirs(&membership.sidechain_dirs, &owner,
            &Cancellation::default(), now_ms() + 30_000, proof.as_ref());
        assert_eq!(old.as_ref().map_err(|error| error.status), new.as_ref().map_err(|error| error.status));
        drop(proof);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_sidechain_batch_open_positive_prefix_is_not_absence_authority() {
    let source = Source::new(&format!("{}\n", user("root")));
    let store = store();
    let owner = context("sidechain-open-prefix");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let id = source.path.file_stem().unwrap().to_str().unwrap().to_owned();
    let mut request = catalog_missing_request(&source, &root, &[id]);
    request.insert("roots", json!([source.directory, source.path]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let membership = catalog_cached_membership(&store, &request, &owner, true);
    assert!(membership.complete);
    let proof = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
        &owner, &Cancellation::default(), now_ms() + 30_000).unwrap();
    assert!(proof.is_none());
    store.assert_conserved();
}

#[test]
fn registered_catalog_sidechain_batch_creation_during_file_validation_fails_the_final_catalog_fence() {
    let (_source, store, owner, request, membership) = sidechain_batch_fixture(2);
    let created = Arc::new(AtomicBool::new(false));
    *store.resolve_hook.lock().unwrap() = Some(Arc::new({
        let (trigger, path, created) = (membership.members[0].path.clone(), membership.sidechain_dirs[0].0.clone(), Arc::clone(&created));
        move |path_checked| {
            if path_checked == trigger && !created.swap(true, Ordering::Relaxed) {
                std::fs::create_dir_all(&path).unwrap();
            }
        }
    }));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let result = store.registered_membership(&key, &request, &owner, &Cancellation::default(),
        &mut [0; 20], &mut remaining, true);
    assert!(created.load(Ordering::Relaxed));
    assert!(matches!(result, Err(error) if error.status == Status::Changed));
    store.assert_conserved();
}

#[test]
fn registered_catalog_sidechain_batch_scratch_refusal_leaves_the_original_probe_path_available() {
    let (_source, store, owner, _request, membership) = sidechain_batch_fixture(3);
    let cap = store.memory_cap(&owner).unwrap();
    let filler = store.reserve_projection(&owner, cap - store.retained_accounted_bytes() - 1).unwrap();
    let before = store.retained_accounted_bytes();
    let proof = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
        &owner, &Cancellation::default(), now_ms() + 30_000).unwrap();
    assert!(proof.is_none());
    assert_eq!(store.retained_accounted_bytes(), before);
    let probes = store.sidechain_path_probes.load(Ordering::Relaxed);
    assert!(store.validate_sidechain_dirs(&membership.sidechain_dirs, &owner,
        &Cancellation::default(), now_ms() + 30_000, None).unwrap());
    assert_eq!(store.sidechain_path_probes.load(Ordering::Relaxed) - probes, 3);
    drop(filler);
    store.assert_conserved();
}

#[test]
fn registered_catalog_sidechain_batch_cancel_restores_the_catalog_and_releases_scratch() {
    let (_source, store, owner, _request, membership) = sidechain_batch_fixture(3);
    let before = store.retained_accounted_bytes();
    let cancel = Cancellation::default();
    *store.location_metadata_hook.lock().unwrap() = Some(("sidechain_batch_name", Arc::new({
        let cancel = cancel.clone(); move || cancel.cancel()
    })));
    let result = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
        &owner, &cancel, now_ms() + 30_000);
    assert!(matches!(result, Err(error) if error.status == Status::Cancelled));
    assert_eq!(store.retained_accounted_bytes(), before);
    store.assert_conserved();
    let proof = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
        &owner, &Cancellation::default(), now_ms() + 30_000).unwrap();
    assert!(proof.is_some());
    drop(proof);
    store.assert_conserved();
}

#[test]
fn registered_catalog_sidechain_batch_does_not_borrow_a_catalog_from_another_authority() {
    let (_source, store, owner, _request, membership) = sidechain_batch_fixture(2);
    let mut restricted = owner.clone();
    restricted.insert("authority", json!({"kind":"restricted_roots","roots":[],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let proof = store.sidechain_absences(membership.catalog, &membership.sidechain_dirs,
        &restricted, &Cancellation::default(), now_ms() + 30_000).unwrap();
    assert!(proof.is_none());
    store.assert_conserved();
}

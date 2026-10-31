#[test]
fn registered_catalog_uid_shared_physical_cache_never_reuses_a_broader_context_grant() {
    let source = Source::new(&format!("{}\n",user("root")));
    let target = Source::new(&format!("{}\n",user("wanted")));
    let forest = source.directory.join("forest");
    std::fs::create_dir(&forest).unwrap();
    std::os::unix::fs::symlink(&target.path,forest.join("wanted.jsonl")).unwrap();
    let store = store();
    let owner = context("registered-current-context");
    let root = finish(&store,store.request(&acquire(&source.path),&owner,&Cancellation::default()),&owner);
    let mut request = catalog_missing_request(&source,&root,&["wanted".to_owned()]);
    request.insert("roots",json!([forest]));
    let (cold,cold_entries) = catalog_counted_request(&store,&request,&owner,&Cancellation::default());
    assert_eq!(cold["status"].as_str(),Some("ok"),"{cold:?}");
    assert_eq!(cold_entries,1);
    request.insert("deadline_unix_ms",json!(now_ms()+750));
    let (warm,entries) = catalog_counted_request(&store,&request,&owner,&Cancellation::default());
    assert_eq!(warm["status"].as_str(),Some("ok"),"{warm:?}");
    assert_eq!(entries,0);
    let mut narrowed = owner.clone();
    narrowed.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let roots = [json!(forest)];
    let (broad_key,broad_reservation) = store.catalog_key(&roots,&owner).unwrap();
    let (narrow_key,narrow_reservation) = store.catalog_key(&roots,&narrowed).unwrap();
    assert_eq!(broad_key,narrow_key,"test must exercise one UID-shared physical owner");
    drop(broad_reservation); drop(narrow_reservation);
    let before = store.directory_descriptor_metadata.load(Ordering::Relaxed);
    request.insert("deadline_unix_ms",json!(now_ms()+750));
    let (denied,_) = catalog_counted_request(&store,&request,&narrowed,&Cancellation::default());
    assert_eq!(denied["status"].as_str(),Some("permission_denied"),"{denied:?}");
    assert!(denied["data"].is_null() && denied["cursor"].is_null());
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),before,"CURRENT denied target precedes descriptor object metadata");
    {
        let state = store.lock_state();
        assert_eq!(state.catalogs.len(),1);
        let slot = state.catalogs.get(&broad_key).unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled shared physical catalog"); };
        assert_eq!(catalog.charge(),catalog.audit_charge());
    }
    request.insert("deadline_unix_ms",json!(now_ms()+750));
    let (recovered,_) = catalog_counted_request(&store,&request,&owner,&Cancellation::default());
    assert_eq!(recovered["status"].as_str(),Some("ok"),"{recovered:?}");
    request.insert("deadline_unix_ms",json!(now_ms()+750));
    let (again,entries) = catalog_counted_request(&store,&request,&owner,&Cancellation::default());
    assert_eq!(again["status"].as_str(),Some("ok"),"{again:?}");
    assert_eq!(entries,0,"a private refusal cannot become retained global denial");
    store.assert_conserved();
}

#[test]
fn registered_catalog_certificate_rejects_a_regular_root_retarget_after_public_lookup() {
    let source = Source::new(&format!("{}\n", user("root")));
    let original = Source::new(&format!("{}\n", user("original-member")));
    let replacement = Source::new(&format!("{}\n", user("replacement-member")));
    let alias = source.directory.join("wanted.jsonl");
    std::os::unix::fs::symlink(&original.path, &alias).unwrap();
    let store = store();
    let owner = context("catalog-public-certificate");
    let mut locate = public_catalog_request(&source, &["missing"]);
    locate.insert("roots", json!([alias]));
    let (negative, entries) = public_catalog_finish(&store, &locate, &owner);
    assert_eq!(negative[0]["status"].as_str(), Some("missing"));
    assert_eq!(entries, 0);
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = registered_prepare(&source, &root, &["wanted"]);
    request.insert("roots", json!([alias]));
    let first = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let initial = store.lock_state().warm_memberships.get(&key).cloned().expect("initial registered membership");
    let certificate = initial.catalog.expect("initial catalog certificate");
    assert_eq!(initial.members.len(), 1);
    assert_eq!(initial.members[0].path, std::fs::canonicalize(&original.path).unwrap());
    release_graph(&store, &first, &owner);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&replacement.path, &alias).unwrap();
    assert_eq!(SourceStamp::of(&std::fs::metadata(&original.path).unwrap()), initial.members[0].stamp);
    locate.insert("session_ids", json!(["unhinted-missing"]));
    let (negative, entries) = public_catalog_finish(&store, &locate, &owner);
    assert_eq!(negative[0]["status"].as_str(), Some("missing"));
    assert_eq!(entries, 0);
    {
        let state = store.lock_state();
        let slot = state.catalogs.get(certificate.key()).expect("shared public catalog");
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled public catalog"); };
        assert!(!certificate.matches(catalog), "public root hold did not invalidate the registered certificate");
    }
    let second = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
    let fresh = store.lock_state().warm_memberships.get(&key).cloned().expect("updated registered membership");
    assert_eq!(fresh.members.len(), 1);
    assert_eq!(fresh.members[0].path, std::fs::canonicalize(&replacement.path).unwrap());
    assert_ne!(fresh.catalog, Some(certificate));
    assert_eq!(SourceStamp::of(&std::fs::metadata(&original.path).unwrap()), initial.members[0].stamp);
    release_graph(&store, &second, &owner);
    store.assert_conserved();
}

#[test]
fn registered_catalog_certificate_is_authority_bound_inside_one_physical_owner() {
    let source = Source::new(&format!("{}\n", user("root")));
    std::fs::write(source.directory.join("wanted.jsonl"), format!("{}\n", user("wanted"))).unwrap();
    let store = store();
    let owner = context("catalog-certificate-authority");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    let membership = catalog_cached_membership(&store, &request, &owner, true);
    let certificate = membership.catalog.unwrap();
    let mut restricted = owner.clone();
    restricted.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let roots = request["roots"].as_array().unwrap();
    let (broad_key, broad_reservation) = store.catalog_key(roots, &owner).unwrap();
    let (restricted_key, restricted_reservation) = store.catalog_key(roots, &restricted).unwrap();
    assert_eq!(broad_key, restricted_key);
    drop(broad_reservation);
    drop(restricted_reservation);
    let key = NativeStore::warm_membership_key(&request, &restricted).unwrap();
    let result = store.publish_warm_membership(&key, &membership, &restricted, None);
    assert!(matches!(result, Err(error) if error.status == Status::Changed));
    assert!(!store.lock_state().warm_memberships.contains_key(&key));
    let mut remaining = limits(&request).unwrap();
    assert!(!store.catalog_membership_valid(Some(certificate), &request, &restricted,
        &Cancellation::default(), &mut remaining).unwrap());
    let mut remaining = limits(&request).unwrap();
    assert!(store.catalog_membership_valid(Some(certificate), &request, &owner,
        &Cancellation::default(), &mut remaining).unwrap());
    store.assert_conserved();
}

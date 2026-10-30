fn public_review_store(step: usize) -> NativeStore {
    NativeStore::new(&json!({"max_events_per_step":step,"max_retained_bytes":CAP,"reserved_hook_accounted_bytes":HOOK_BYTES})).unwrap()
}

fn public_review_request(source: &LedgerSource, ids: &[&str]) -> Value {
    let template = acquire(&source.path);
    let mut bounds = template["limits"].clone();
    bounds.insert("max_discovery_entries",json!(50_000));
    bounds.insert("max_sources",json!(1024));
    bounds.insert("max_items",json!(1024));
    json!({"schema":SCHEMA,"id":"public-review","operation":"locate","session_ids":ids,"roots":[source.directory],"deadline_unix_ms":now_ms()+30_000,"limits":bounds})
}

fn public_review_finish(store: &NativeStore, request: &Value, owner: &Value) -> (Value,Vec<Value>) {
    let mut response = store.request(request,owner,&Cancellation::default());
    let mut rows = Vec::new();
    for _ in 0..4096 {
        assert!(matches!(response["status"].as_str(),Some("ok"|"incomplete")),"{response:?}");
        rows.extend(response["data"]["sessions"].as_array().unwrap().iter().cloned());
        let Some(cursor) = response["cursor"].as_str() else { return (response,rows); };
        response = store.request(&json!({"schema":SCHEMA,"id":"public-review-resume","operation":"resume","cursor":cursor}),owner,&Cancellation::default());
    }
    panic!("public review request did not settle");
}

#[test]
fn public_catalog_review_an_observed_error_precedes_a_later_positive() {
    let source = LedgerSource::new("");
    let later = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    std::os::unix::fs::symlink("loop",source.directory.join("loop")).unwrap();
    let later_wanted = later.file("wanted.jsonl","");
    let created = source.directory.join("wanted.jsonl");
    let created_by_hook = created.clone();
    let store = public_review_store(1);
    let owner = context_for("public-review-error",false);
    *store.location_metadata_hook.lock().unwrap() = Some(("entry",Arc::new(move || {
        std::fs::write(&created_by_hook,"").unwrap();
    })));
    let mut request = public_review_request(&source,&["wanted"]);
    request.insert("roots",json!([later_wanted,source.directory]));
    let response = store.request(&request,&owner,&Cancellation::default());
    assert!(created.exists());
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(),Some(1),"{response:?}");
    assert_eq!(response["status"].as_str(),Some("changed"),"{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

fn public_review_entry_attempt(headroom: usize, verify_recovery: bool) -> bool {
    let source = LedgerSource::new("");
    let store: &'static NativeStore = Box::leak(Box::new(public_review_store(128)));
    let owner = context_for("public-review-entry-admission",false);
    let request = public_review_request(&source,&["s"]);
    let held: Arc<Mutex<Option<ProjectionReservation<'static>>>> = Arc::new(Mutex::new(None));
    let pinned: Arc<Mutex<Option<Arc<CatalogSlot>>>> = Arc::new(Mutex::new(None));
    let held_hook = Arc::clone(&held);
    let pinned_hook = Arc::clone(&pinned);
    let filler_owner = owner.clone();
    *store.location_metadata_hook.lock().unwrap() = Some(("entry",Arc::new(move || {
        *pinned_hook.lock().unwrap() = Some(store.lock_state().catalogs.values().next().cloned().expect("active catalog"));
        let available = cap_of(store,&filler_owner) - ledger(store)[TOTAL];
        assert!(headroom <= available,"requested headroom {headroom} exceeds {available}");
        *held_hook.lock().unwrap() = Some(store.reserve_projection(&filler_owner,available-headroom).unwrap());
    })));
    let response = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(),Some(1),"{response:?}");
    assert!(store.location_metadata_hook.lock().unwrap().is_none());
    let slot = pinned.lock().unwrap().as_ref().cloned().expect("catalog pinned after entry consumption");
    let reached_install = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("catalog did not restore after request"); };
        catalog.pending_raw.is_none() && catalog.pending_edge.is_none()
    };
    assert!(matches!(response["status"].as_str(),Some("ok"|"retained_limit")),"{response:?}");
    store.assert_conserved();
    drop(held.lock().unwrap().take());
    if verify_recovery {
        assert!(reached_install,"the refused request did not reach the post-install boundary: {response:?}");
        assert_eq!(response["status"].as_str(),Some("retained_limit"),"{response:?}");
        assert!(response["cursor"].is_null());
        let (_,rows) = public_review_finish(store,&request,&owner);
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0]["status"].as_str(),Some("ok"),"headroom={headroom} rows={rows:?}");
        assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()),source.path);
        assert!(source.path.exists());
        store.assert_conserved();
    }
    drop(pinned.lock().unwrap().take());
    reached_install
}

#[test]
fn public_catalog_review_a_refused_post_install_allocation_preserves_the_entry() {
    let exact = exact_fit(&|headroom| public_review_entry_attempt(headroom,false));
    assert!(exact > 0);
    assert!(!public_review_entry_attempt(exact-1,false));
    assert!(public_review_entry_attempt(exact,true));
}

#[test]
fn public_catalog_review_external_target_replacement_rewinds_its_generation() {
    let source = LedgerSource::new("");
    let outside = LedgerSource::new("");
    let first_target = outside.file("target-first.jsonl","");
    let second_target = outside.file("target-second.jsonl","");
    std::os::unix::fs::symlink(&first_target,source.directory.join("alias-first.jsonl")).unwrap();
    std::os::unix::fs::symlink(&second_target,source.directory.join("alias-second.jsonl")).unwrap();
    let store = public_review_store(128);
    let owner = context_for("public-review-target-replacement",false);
    let request = public_review_request(&source,&["missing"]);
    let (_,warm) = public_review_finish(&store,&request,&owner);
    assert_eq!(warm[0]["status"].as_str(),Some("missing"));
    let (edge_index,alias,target,previous) = {
        let state = store.lock_state();
        let slot = state.catalogs.values().next().expect("warm catalog");
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("warm catalog is not idle"); };
        let observed = catalog.directories.values().find_map(|node| {
            node.symlinks.iter().copied().find(|index| *index > 0).map(|index| {
                let edge = &node.edges[index];
                (index,edge.name.clone(),edge.binding.canonical.as_ref().unwrap().clone(),edge.binding.stamp.unwrap())
            })
        }).expect("at least one of two observed symlinks must have a nonzero edge index");
        observed
    };
    assert!(edge_index > 0);
    let parent_before = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
    let replacement = outside.file("replacement.jsonl","replacement");
    std::fs::rename(&replacement,&target).unwrap();
    let fresh = SourceStamp::of(&std::fs::metadata(&target).unwrap());
    assert_ne!(previous.identity,fresh.identity);
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()),parent_before);
    let (_,missing) = public_review_finish(&store,&request,&owner);
    assert_eq!(missing[0]["status"].as_str(),Some("missing"));
    let id = Path::new(&alias).file_stem().unwrap().to_str().unwrap();
    let (_,located) = public_review_finish(&store,&public_review_request(&source,&[id]),&owner);
    assert_eq!(located[0]["status"].as_str(),Some("ok"),"{located:?}");
    assert_eq!(Path::new(located[0]["path"].as_str().unwrap()),source.directory.join(alias));
    assert_eq!(located[0]["revision"].as_str(),Some(fresh.revision().as_str()));
    store.assert_conserved();
}

#[test]
fn public_catalog_review_duplicate_directory_roots_keep_each_raw_authority_check() {
    let source = LedgerSource::new("");
    let outside = LedgerSource::new("");
    let aliases = LedgerSource::new("");
    let first = aliases.directory.join("first");
    let second = aliases.directory.join("second");
    std::os::unix::fs::symlink(&source.directory,&first).unwrap();
    std::os::unix::fs::symlink(&source.directory,&second).unwrap();
    let store = Arc::new(public_review_store(128));
    let mut owner = context_for("public-review-duplicate-roots",false);
    owner.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let mut request = public_review_request(&source,&["missing"]);
    request.insert("roots",json!([first,second]));
    let (_,warm) = public_review_finish(&store,&request,&owner);
    assert_eq!(warm[0]["status"].as_str(),Some("missing"));
    let alias = first.clone();
    let retarget = outside.directory.clone();
    let switch: Arc<dyn Fn()+Send+Sync> = Arc::new(move || {
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&retarget,&alias).unwrap();
    });
    let weak = Arc::downgrade(&store);
    *store.location_metadata_hook.lock().unwrap() = Some(("root",Arc::new(move || {
        let store = weak.upgrade().unwrap();
        *store.location_metadata_hook.lock().unwrap() = Some(("root",Arc::clone(&switch)));
    })));
    let response = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(std::fs::canonicalize(&first).unwrap(),std::fs::canonicalize(&outside.directory).unwrap());
    assert_eq!(response["status"].as_str(),Some("permission_denied"),"{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

#[test]
fn public_catalog_review_an_open_alias_uses_the_current_route_after_authority_changes() {
    let source = LedgerSource::new("");
    let outside = LedgerSource::new("");
    for name in ["first.txt","second.txt"] { source.file(name,""); outside.file(name,""); }
    let aliases = LedgerSource::new("");
    let alias = aliases.directory.join("route");
    std::os::unix::fs::symlink(&source.directory,&alias).unwrap();
    let store = public_review_store(1);
    let owner = context_for("public-review-open-alias",false);
    let mut request = public_review_request(&source,&["missing"]);
    request.insert("roots",json!([alias]));
    let first = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(first["status"].as_str(),Some("incomplete"),"{first:?}");
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("open alias cursor");
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&outside.directory,&alias).unwrap();
    let mut narrowed = owner.clone();
    narrowed.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let response = store.request(&json!({"schema":SCHEMA,"id":"public-review-narrowed-alias","operation":"resume","cursor":cursor}),&narrowed,&Cancellation::default());
    assert_eq!(response["status"].as_str(),Some("permission_denied"),"{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

#[test]
fn public_catalog_review_a_rebuilt_ordinal_cannot_transfer_another_alias_capability() {
    let source = LedgerSource::new("");
    let outside = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    for name in ["first.jsonl","second.jsonl"] {
        std::os::unix::fs::symlink(&outside.path,source.directory.join(name)).unwrap();
    }
    let store = public_review_store(1);
    let owner = context_for("public-review-original-capability",false);
    let broad = context_for("public-review-shared-index-writer",false);
    let request = public_review_request(&source,&["missing"]);
    let first = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(first["status"].as_str(),Some("incomplete"),"{first:?}");
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("original alias cursor").to_owned();
    let consumed = {
        let state = store.lock_state();
        let slot = state.locates.get(&cursor).unwrap().catalog.as_ref().unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("original catalog is not idle"); };
        let node = catalog.directories.values().next().expect("indexed alias directory");
        assert_eq!(node.edges.len(),1);
        assert_eq!(node.symlinks.as_slice(),&[0]);
        node.edges[0].name.clone()
    };
    let (_,completed) = public_review_finish(&store,&request,&broad);
    assert_eq!(completed[0]["status"].as_str(),Some("missing"));
    std::fs::remove_file(source.directory.join(&consumed)).unwrap();
    let (_,rebuilt) = public_review_finish(&store,&request,&broad);
    assert_eq!(rebuilt[0]["status"].as_str(),Some("missing"));
    {
        let state = store.lock_state();
        let slot = state.locates.get(&cursor).unwrap().catalog.as_ref().unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("rebuilt catalog is not idle"); };
        let node = catalog.directories.values().next().expect("rebuilt alias directory");
        assert_eq!(node.edges.len(),1);
        assert_ne!(node.edges[0].name,consumed);
        assert_eq!(node.edges[0].binding.canonical.as_ref(),Some(&std::fs::canonicalize(&outside.path).unwrap()));
    }
    let mut narrowed = owner.clone();
    narrowed.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let response = store.request(&json!({"schema":SCHEMA,"id":"public-review-rebuilt-alias-resume","operation":"resume","cursor":cursor}),&narrowed,&Cancellation::default());
    assert_eq!(response["status"].as_str(),Some("permission_denied"),"{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

#[test]
fn public_catalog_review_numeric_uid_spellings_keep_the_same_owner_and_budget() {
    let source = LedgerSource::new("");
    let store = public_review_store(1);
    let owner = context_for("public-review-numeric-owner",false);
    let mut request = public_review_request(&source,&["missing"]);
    request["limits"].insert("max_discovery_entries",json!(2));
    let first = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(first["status"].as_str(),Some("incomplete"),"{first:?}");
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    let cursor = first["cursor"].as_str().expect("numeric owner cursor").to_owned();
    let original_slot = store.lock_state().locates.get(&cursor).unwrap().catalog.as_ref().cloned().unwrap();
    let mut same_owner = owner.clone();
    same_owner["authority"].insert("effective_uid",json!(format!("0{}",unsafe { libc::geteuid() })));
    let response = store.request(&json!({"schema":SCHEMA,"id":"public-review-leading-zero-resume","operation":"resume","cursor":cursor}),&same_owner,&Cancellation::default());
    assert_eq!(response["status"].as_str(),Some("ok"),"{response:?}");
    assert!(response["cursor"].is_null());
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(),Some(0));
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64().unwrap()+response["usage"]["discovery_entries_examined"].as_u64().unwrap(),1);
    assert_eq!(response["data"]["sessions"][0]["session_id"].as_str(),Some("missing"));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(),Some("missing"));
    {
        let state = store.lock_state();
        assert_eq!(state.catalogs.len(),1);
        assert!(Arc::ptr_eq(state.catalogs.values().next().unwrap(),&original_slot));
    }
    store.assert_conserved();
}

fn public_review_ordinary_error_batch(headroom: Option<usize>) -> bool {
    const ERRORS: usize = 8;
    let source = LedgerSource::new(&line("ordinary-error-root"));
    let inputs = source.directory.join("ordinary-inputs");
    std::fs::create_dir(&inputs).unwrap();
    let names: HashSet<_> = (0..=ERRORS).map(|index| {
        let name = OsString::from(format!("ordinary-{index:02}.txt"));
        std::fs::write(inputs.join(&name),"").unwrap();
        name
    }).collect();
    let store = Arc::new(public_review_store(ERRORS));
    let mut owner = context_for("public-review-ordinary-errors",false);
    // Restricted authority must resolve every ordinary entry; the user-authority
    // dual-inode parent proof intentionally avoids these full-path resolutions.
    owner.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let (root,_pin) = acquired(&store,&source.path,&owner);
    let mut seed = public_review_request(&source,&["missing"]);
    seed.insert("roots",json!([inputs]));
    seed["limits"].insert("max_discovery_entries",json!(1));
    let seeded = store.request(&seed,&owner,&Cancellation::default());
    assert_eq!(seeded["status"].as_str(),Some("incomplete"),"{seeded:?}");
    assert!(seeded["cursor"].is_null());
    assert_eq!(seeded["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    let (slot,key,neutral,before) = {
        let state = store.lock_state();
        assert_eq!(state.catalogs.len(),1);
        let (key,slot) = state.catalogs.iter().next().unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("seeded ordinary catalog is not idle"); };
        let node = catalog.directories.values().next().unwrap();
        assert_eq!(node.edges.len(),1);
        assert!(node.symlinks.is_empty());
        assert!(matches!(node.scan,CatalogScan::Open(_)));
        let neutral = node.edges[0].name.clone();
        drop(cell);
        (Arc::clone(slot),key.clone(),neutral,NativeStore::audit_catalog_bytes(key,slot))
    };
    let targets: HashSet<_> = names.into_iter().filter(|name| *name != neutral).collect();
    assert_eq!(targets.len(),ERRORS);
    let canonical_inputs = std::fs::canonicalize(&inputs).unwrap();
    let observed: Arc<Mutex<Vec<OsString>>> = Arc::new(Mutex::new(Vec::new()));
    let batches: Arc<Mutex<Vec<Vec<Trace>>>> = Arc::new(Mutex::new(Vec::new()));
    let sealed = Arc::new(AtomicBool::new(false));
    let observed_hook = Arc::clone(&observed);
    let batches_hook = Arc::clone(&batches);
    let sealed_hook = Arc::clone(&sealed);
    let weak = Arc::downgrade(&store);
    *store.canonical_error_hook.lock().unwrap() = Some(Arc::new(move |path| {
        let store = weak.upgrade().expect("live ordinary-error fixture");
        if path.file_name().is_some_and(|name| targets.contains(name)) {
            assert_eq!(std::fs::canonicalize(path.parent().unwrap()).unwrap(),canonical_inputs);
            let mut observed = observed_hook.lock().unwrap();
            if observed.len() < ERRORS {
                let trace = traced(&store);
                if !observed.is_empty() { batches_hook.lock().unwrap().push(trace); }
                observed.push(path.file_name().unwrap().to_os_string());
            }
            return Some(std::io::Error::new(std::io::ErrorKind::Other,"injected ordinary canonical error"));
        }
        if observed_hook.lock().unwrap().len() == ERRORS && !sealed_hook.swap(true,Ordering::Relaxed) {
            batches_hook.lock().unwrap().push(traced(&store));
        }
        None
    }));
    let filler = headroom.map(|headroom| fill_to(&store,&owner,headroom));
    let request = prepare_request(&root,&["missing".to_owned()],&[inputs.to_string_lossy().into_owned()],&[]);
    let response = store.request(&request,&owner,&Cancellation::default());
    *store.canonical_error_hook.lock().unwrap() = None;
    assert!(matches!(response["status"].as_str(),Some("changed"|"retained_limit")),"{response:?}");
    let committed = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("ordinary-error owner did not settle"); };
        let node = catalog.directories.values().next().unwrap();
        assert!(node.symlinks.is_empty());
        node.edges.len() == ERRORS+1 && node.issues.len() == ERRORS
    };
    if committed {
        assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(),Some(ERRORS as u64));
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(),ERRORS);
        assert_eq!(observed.iter().collect::<HashSet<_>>().len(),ERRORS);
        assert!(sealed.load(Ordering::Relaxed));
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(),ERRORS);
        assert!(batches.iter().all(|batch| reserved_traces(batch).len() == 1),"{batches:?}");
        let reserved: usize = batches.iter().flat_map(|batch| reserved_traces(batch)).sum();
        let after = NativeStore::audit_catalog_bytes(&key,&slot);
        assert_eq!(after-before,reserved,
            "the ordinary-error slice did not pre-admit all persistent edge and ordered-check storage");
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { unreachable!("settled ordinary catalog"); };
        let node = catalog.directories.values().next().unwrap();
        assert!(matches!(node.scan,CatalogScan::Open(_)),"the slice unexpectedly released its directory handle");
        assert_eq!(catalog.charge(),catalog.audit_charge());
    }
    if headroom.is_none() {
        assert!(committed);
        assert_eq!(response["status"].as_str(),Some("changed"),"{response:?}");
        assert!(response["data"].is_null());
        assert!(response["cursor"].is_null());
    }
    store.assert_conserved();
    drop(filler);
    store.assert_conserved();
    committed
}

#[test]
fn public_catalog_review_ordinary_io_errors_pre_admit_the_whole_registered_slice() {
    assert!(public_review_ordinary_error_batch(None));
    let exact = exact_fit(&|headroom| public_review_ordinary_error_batch(Some(headroom)));
    assert!(exact > 0);
    assert!(!public_review_ordinary_error_batch(Some(exact-1)));
    assert!(public_review_ordinary_error_batch(Some(exact)));
}

#[test]
fn public_catalog_review_narrowed_resume_checks_an_unconsumed_shared_regular_entry() {
    let source = LedgerSource::new("");
    let allowed = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..128 { source.file(&format!("ordinary-{index:03}.txt"), ""); }
    let store = public_review_store(1);
    let owner = context_for("public-review-shared-regular", false);
    let request = public_review_request(&source, &["missing"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    let cursor = first["cursor"].as_str().expect("one-entry private cursor").to_owned();
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let (_, rows) = public_review_finish(&store, &request, &owner);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"));
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let materialized = store.authority_paths_materialized.load(Ordering::Relaxed);
    let response = store.request(&json!({"schema":SCHEMA,"id":"shared-regular-narrowed-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("permission_denied"), "{response:?}");
    assert_eq!(store.authority_paths_materialized.load(Ordering::Relaxed) - materialized, 1);
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

#[test]
fn public_catalog_review_unrelated_root_mutation_keeps_a_consumed_prefix() {
    let source = LedgerSource::new("");
    let other = LedgerSource::new("");
    let positive = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    source.file("consumed.txt", "");
    let wanted = positive.file("wanted.jsonl", "");
    let store = public_review_store(1);
    let owner = context_for("public-review-unrelated-root", false);
    let mut request = public_review_request(&source, &["wanted"]);
    request.insert("roots", json!([other.directory, wanted, source.directory]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert!(first["data"]["sessions"].as_array().unwrap().is_empty());
    let cursor = first["cursor"].as_str().expect("consumed prefix cursor").to_owned();
    let mut missing = request.clone();
    missing.insert("session_ids", json!(["missing"]));
    public_review_finish(&store, &missing, &owner);
    other.file("added.txt", "");
    public_review_finish(&store, &missing, &owner);
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[positive.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let mut response = store.request(&json!({"schema":SCHEMA,"id":"unrelated-root-narrowed-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
    for _ in 0..8 {
        assert!(matches!(response["status"].as_str(), Some("ok" | "incomplete")), "{response:?}");
        if let Some(cursor) = response["cursor"].as_str() {
            response = store.request(&json!({"schema":SCHEMA,"id":"unrelated-root-narrowed-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
        } else {
            assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(Path::new(response["data"]["sessions"][0]["path"].as_str().unwrap()), wanted);
            store.assert_conserved();
            return;
        }
    }
    panic!("closed allowed prefix did not settle");
}

#[test]
fn public_catalog_review_a_closed_positive_does_not_materialize_a_later_authority_view() {
    let consumed = LedgerSource::new("");
    let wanted = LedgerSource::new("");
    let later = LedgerSource::new("");
    std::fs::remove_file(&consumed.path).unwrap();
    consumed.file("consumed.txt", "");
    for index in 0..256 { later.file(&format!("later-{index:03}.txt"), ""); }
    let store = public_review_store(1);
    let owner = context_for("public-review-lazy-authority-view", false);
    let mut request = public_review_request(&consumed, &["s"]);
    request.insert("roots", json!([later.directory, wanted.path, consumed.directory]));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let cursor = first["cursor"].as_str().expect("consumed root cursor").to_owned();
    let mut missing = request.clone();
    missing.insert("session_ids", json!(["missing"]));
    public_review_finish(&store, &missing, &owner);
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[wanted.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let materialized = store.authority_paths_materialized.load(Ordering::Relaxed);
    let response = store.request(&json!({"schema":SCHEMA,"id":"lazy-authority-positive-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    assert_eq!(Path::new(response["data"]["sessions"][0]["path"].as_str().unwrap()), wanted.path);
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(0));
    assert_eq!(store.authority_paths_materialized.load(Ordering::Relaxed) - materialized, 0);
    store.assert_conserved();
}

#[test]
fn public_catalog_review_narrowed_resume_checks_unconsumed_children_of_a_symlink() {
    let source = LedgerSource::new("");
    let outside = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    std::fs::remove_file(&outside.path).unwrap();
    outside.file("first.txt", "");
    outside.file("second.txt", "");
    std::os::unix::fs::symlink(&outside.directory, source.directory.join("alias")).unwrap();
    let store = public_review_store(1);
    let owner = context_for("public-review-shared-symlink-children", false);
    let request = public_review_request(&source, &["missing"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    let cursor = first["cursor"].as_str().expect("symlink entry cursor");
    let second = store.request(&json!({"schema":SCHEMA,"id":"consume-alias-child","operation":"resume","cursor":cursor}), &owner, &Cancellation::default());
    assert_eq!(second["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    let cursor = second["cursor"].as_str().expect("first ordinary child cursor").to_owned();
    public_review_finish(&store, &request, &owner);
    let mut narrowed = owner.clone();
    narrowed.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let response = store.request(&json!({"schema":SCHEMA,"id":"shared-alias-narrowed-resume","operation":"resume","cursor":cursor}), &narrowed, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("permission_denied"), "{response:?}");
    assert!(response["data"].is_null());
    assert!(response["cursor"].is_null());
    store.assert_conserved();
}

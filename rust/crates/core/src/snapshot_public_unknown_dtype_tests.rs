fn unknown_dtype_store() -> NativeStore {
    let store = NativeStore::new(&json!({"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    store.directory_force_unknown.store(true,Ordering::Relaxed);
    store
}

fn unknown_dtype_restricted(claimant: &str, root: &Path) -> Value {
    let mut owner = context(claimant);
    owner.insert("authority",json!({"kind":"restricted_roots","roots":[root],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    owner
}

fn unknown_dtype_collect(store: &NativeStore, mut response: Value, owner: &Value) -> (Value,Vec<Value>,u64) {
    let mut rows = Vec::new();
    let mut entries = 0;
    for _ in 0..4096 {
        assert!(matches!(response["status"].as_str(),Some("ok"|"incomplete")),"{response:?}");
        entries += response["usage"]["discovery_entries_examined"].as_u64().unwrap();
        rows.extend(response["data"]["sessions"].as_array().unwrap().iter().cloned());
        let Some(cursor) = response["cursor"].as_str() else { return (response,rows,entries); };
        response = store.request(&json!({"schema":SCHEMA,"id":"unknown-resume","operation":"resume","cursor":cursor}),owner,&Cancellation::default());
    }
    panic!("unknown dtype query did not settle");
}

#[test]
fn public_catalog_unknown_dtype_resumed_alias_never_stats_the_now_forbidden_old_descriptor() {
    let old = Source::new("");
    let new = Source::new("");
    std::fs::remove_file(&old.path).unwrap();
    std::fs::remove_file(&new.path).unwrap();
    for name in ["first.txt","second.txt"] { std::fs::write(old.directory.join(name),"").unwrap(); }
    let names: Vec<_> = std::fs::read_dir(&old.directory).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    for name in &names { std::fs::write(new.directory.join(name),"").unwrap(); }
    let alias_holder = Source::new("");
    let alias = alias_holder.directory.join("alias");
    std::os::unix::fs::symlink(&old.directory,&alias).unwrap();
    let store = unknown_dtype_store();
    let owner = context("unknown-alias");
    let mut request = public_catalog_request(&old,&["absent"]);
    request.insert("roots",json!([alias]));
    let first = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    let cursor = first["cursor"].as_str().expect("old A name iterator").to_owned();
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),1,"authorized UNKNOWN control must execute a real descriptor type read");
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&new.directory,&alias).unwrap();
    let narrowed = unknown_dtype_restricted("unknown-alias",&new.directory);
    let before = store.directory_descriptor_metadata.load(Ordering::Relaxed);
    let parent_before = store.directory_parent_proof_metadata.load(Ordering::Relaxed);
    let resumed = store.request(&json!({"schema":SCHEMA,"id":"unknown-narrow","operation":"resume","cursor":cursor}),&narrowed,&Cancellation::default());
    let (last,rows,entries) = unknown_dtype_collect(&store,resumed,&narrowed);
    assert_eq!(last["status"].as_str(),Some("ok"),"{last:?}");
    assert_eq!(rows.len(),1);
    assert_eq!(rows[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,1);
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),before,"the old A descriptor cannot lend metadata authority to a B logical observation");
    assert_eq!(store.directory_parent_proof_metadata.load(Ordering::Relaxed),parent_before,"optional cache publication cannot stat the forbidden old parent");
    store.assert_conserved();
}

#[test]
fn public_catalog_unknown_dtype_symlink_denial_precedes_descriptor_metadata_and_allowed_reuse_keeps_its_guard() {
    let source = Source::new("");
    let outside = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let wanted = source.directory.join("wanted.jsonl");
    std::os::unix::fs::symlink(&outside.path,&wanted).unwrap();
    let store = unknown_dtype_store();
    let restricted = unknown_dtype_restricted("unknown-denied",&source.directory);
    let request = public_catalog_request(&source,&["wanted"]);
    let denied = store.request(&request,&restricted,&Cancellation::default());
    assert_eq!(denied["status"].as_str(),Some("permission_denied"),"{denied:?}");
    assert!(denied["data"].is_null());
    assert_eq!(denied["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),0);
    let owner = context("unknown-allowed");
    let (last,rows,entries) = unknown_dtype_collect(&store,store.request(&request,&owner,&Cancellation::default()),&owner);
    assert_eq!(last["status"].as_str(),Some("ok"),"{last:?}");
    assert_eq!(rows[0]["status"].as_str(),Some("ok"));
    assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()),wanted);
    assert_eq!(entries,1);
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),1);
    {
        let state = store.lock_state();
        let slot = state.catalogs.values().next().unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled catalog"); };
        let canonical = std::fs::canonicalize(&source.directory).unwrap();
        let node = catalog.directories.values().find(|node| node.canonical == canonical).unwrap();
        assert_eq!(node.symlinks.as_slice(),&[0],"UNKNOWN must resolve as a guarded symlink, never as false");
    }
    store.assert_conserved();
}

#[test]
fn public_catalog_unknown_dtype_discovery_resolves_logical_type_only_after_current_target_authority() {
    let source = Source::new("");
    let outside = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    std::os::unix::fs::symlink(&outside.path,source.directory.join("wanted.jsonl")).unwrap();
    let store = unknown_dtype_store();
    let restricted = unknown_dtype_restricted("unknown-discover-denied",&source.directory);
    let template = public_catalog_request(&source,&["absent"]);
    let request = json!({"schema":SCHEMA,"id":"unknown-discover","operation":"discover","roots":[source.directory],"checkpoint":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
    let denied = store.request(&request,&restricted,&Cancellation::default());
    assert_eq!(denied["status"].as_str(),Some("permission_denied"),"{denied:?}");
    assert_eq!(denied["usage"]["discovery_entries_examined"].as_u64(),Some(1));
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),0);
    assert_eq!(store.directory_logical_type_metadata.load(Ordering::Relaxed),0);
    let owner = context("unknown-discover-allowed");
    let allowed = store.request(&request,&owner,&Cancellation::default());
    assert!(matches!(allowed["status"].as_str(),Some("ok"|"incomplete")),"{allowed:?}");
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),0);
    assert_eq!(store.directory_logical_type_metadata.load(Ordering::Relaxed),1);
    store.assert_conserved();
}

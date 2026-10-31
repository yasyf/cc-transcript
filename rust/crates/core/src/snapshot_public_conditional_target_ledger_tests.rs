#[test]
fn location_conditional_object_target_and_error_payloads_match_independent_ledger_audit() {
    for dangling in [false,true] {
        let source = LedgerSource::new("");
        let target = LedgerSource::new("");
        std::fs::remove_file(&source.path).unwrap();
        if dangling { std::fs::remove_file(&target.path).unwrap(); }
        std::os::unix::fs::symlink(&target.path,source.directory.join("link.jsonl")).unwrap();
        let store = public_review_store(128);
        let mut owner = context_for("conditional-ledger",false);
        owner.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory,target.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
        let request = public_review_request(&source,&["absent"]);
        let (_,cold) = public_review_finish(&store,&request,&owner);
        assert_eq!(cold[0]["status"].as_str(),Some("missing"));
        assert_eq!(ledger(&store),audited(&store));
        {
            let state = store.lock_state();
            let slot = state.catalogs.values().next().unwrap();
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("settled conditional ownership"); };
            let parent = std::fs::canonicalize(&source.directory).unwrap();
            let node = catalog.directories.values().find(|node| node.canonical == parent).unwrap();
            let edge = node.edges.iter().find(|edge| edge.name == OsStr::new("link.jsonl")).unwrap();
            let CatalogNameBinding::Conditional(target) = &edge.binding else { panic!("owned physical object and conditional target"); };
            assert!(target.object.metadata.symlink);
            assert!(target.object.payload.is_none());
            assert_eq!(edge.issue.is_some(),dangling);
        }
        std::fs::write(&target.path," ").unwrap();
        let (warm,rows) = public_review_finish(&store,&request,&owner);
        assert_eq!(rows[0]["status"].as_str(),Some("missing"));
        assert_eq!(warm["usage"]["discovery_entries_examined"].as_u64(),Some(0));
        assert_eq!(ledger(&store),audited(&store));
        let mut denied = owner.clone();
        denied.insert("authority",json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
        let refused = store.request(&request,&denied,&Cancellation::default());
        assert_eq!(refused["status"].as_str(),Some("permission_denied"),"{refused:?}");
        assert_eq!(ledger(&store),audited(&store));
        let (_,again) = public_review_finish(&store,&request,&owner);
        assert_eq!(again[0]["status"].as_str(),Some("missing"));
        assert_eq!(ledger(&store),audited(&store));
        store.assert_conserved();
    }
}

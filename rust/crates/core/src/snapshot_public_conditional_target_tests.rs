fn conditional_target_owner(id: &str, roots: &[&Path]) -> Value {
    let mut owner = context(id);
    owner.insert("authority",json!({"kind":"restricted_roots","roots":roots,"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    owner
}

fn conditional_target_appends(store: &NativeStore) -> u64 {
    let state = store.lock_state();
    state.catalogs.values().map(|slot| {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled conditional catalog"); };
        catalog.physical_append_work
    }).sum()
}

#[test]
fn public_catalog_conditional_relative_hardlinked_objects_keep_distinct_current_target_routes() {
    for unknown in [false,true] {
        let a = Source::new("");
        let b = Source::new("");
        std::fs::remove_file(&a.path).unwrap();
        std::fs::remove_file(&b.path).unwrap();
        for source in [&a,&b] { std::fs::write(source.directory.join("target"),"").unwrap(); }
        let link_a = a.directory.join("link.jsonl");
        let link_b = b.directory.join("link.jsonl");
        std::os::unix::fs::symlink("target",&link_a).unwrap();
        std::fs::hard_link(&link_a,&link_b).unwrap();
        assert!(SourceStamp::of(&std::fs::symlink_metadata(&link_a).unwrap()) == SourceStamp::of(&std::fs::symlink_metadata(&link_b).unwrap()));
        let store = store();
        store.directory_force_unknown.store(unknown,Ordering::Relaxed);
        let owner = conditional_target_owner("conditional-relative",&[&a.directory,&b.directory]);
        let mut request = public_catalog_request(&a,&["absent"]);
        request.insert("roots",json!([a.directory,b.directory]));
        let (cold,entries) = public_catalog_finish(&store,&request,&owner);
        assert_eq!(cold[0]["status"].as_str(),Some("missing"));
        assert_eq!(entries,4);
        let appended = conditional_target_appends(&store);
        for _ in 0..2 {
            let (warm,entries) = public_catalog_finish(&store,&request,&owner);
            assert_eq!(warm[0]["status"].as_str(),Some("missing"));
            assert_eq!(entries,0);
            assert_eq!(conditional_target_appends(&store),appended);
        }
        {
            let state = store.lock_state();
            let slot = state.catalogs.values().next().unwrap();
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("settled routes"); };
            let mut routes = Vec::new();
            for source in [&a,&b] {
                let parent = std::fs::canonicalize(&source.directory).unwrap();
                let node = catalog.directories.values().find(|node| node.canonical == parent).unwrap();
                let name = node.edges.iter().find(|edge| edge.name == OsStr::new("link.jsonl")).unwrap();
                let CatalogNameBinding::Conditional(target) = &name.binding else { panic!("link target cannot be a physical entry fact"); };
                assert!(target.object.metadata.symlink);
                assert_eq!(target.binding.canonical.as_ref().unwrap(),&std::fs::canonicalize(source.directory.join("target")).unwrap());
                routes.push(target.route);
            }
            assert!(routes[0] != routes[1],"hard-linked relative objects need distinct parent/route target edges");
        }
        store.assert_conserved();
    }
}

#[test]
fn public_catalog_conditional_unchanged_dangling_object_recovers_new_target_without_name_replay() {
    for unknown in [false,true] {
        let source = Source::new("");
        let target = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        std::fs::remove_file(&target.path).unwrap();
        let link = source.directory.join("wanted.jsonl");
        std::os::unix::fs::symlink(&target.path,&link).unwrap();
        let parent_stamp = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
        let object_stamp = SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap());
        let store = store();
        store.directory_force_unknown.store(unknown,Ordering::Relaxed);
        let owner = conditional_target_owner("conditional-created-target",&[&source.directory,&target.directory]);
        let request = public_catalog_request(&source,&["wanted"]);
        let (missing,entries) = public_catalog_finish(&store,&request,&owner);
        assert_eq!(missing[0]["status"].as_str(),Some("missing"));
        assert_eq!(entries,1);
        let appends = conditional_target_appends(&store);
        let (warm,entries) = public_catalog_finish(&store,&request,&owner);
        assert_eq!(warm[0]["status"].as_str(),Some("missing"));
        assert_eq!(entries,0);
        std::fs::write(&target.path,"").unwrap();
        assert!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()) == parent_stamp);
        assert!(SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap()) == object_stamp);
        let (found,entries) = public_catalog_finish(&store,&request,&owner);
        assert_eq!(found[0]["status"].as_str(),Some("ok"));
        assert_eq!(Path::new(found[0]["path"].as_str().unwrap()),link);
        assert_eq!(entries,0);
        assert_eq!(conditional_target_appends(&store),appends);
        let narrowed = conditional_target_owner("conditional-created-target",&[&source.directory]);
        let negative = public_catalog_request(&source,&["absent"]);
        let before = store.directory_descriptor_metadata.load(Ordering::Relaxed);
        let denied = store.request(&negative,&narrowed,&Cancellation::default());
        assert_eq!(denied["status"].as_str(),Some("permission_denied"),"{denied:?}");
        assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),before,"denied target must precede any link-object descriptor read");
        let (allowed,entries) = public_catalog_finish(&store,&negative,&owner);
        assert_eq!(allowed[0]["status"].as_str(),Some("missing"));
        assert_eq!(entries,0);
        assert_eq!(conditional_target_appends(&store),appends);
        store.assert_conserved();
    }
}

#[test]
fn public_catalog_conditional_target_append_refreshes_full_stamp_without_name_replay() {
    let source = Source::new("");
    let target = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let link = source.directory.join("link.jsonl");
    std::os::unix::fs::symlink(&target.path,&link).unwrap();
    let store = store();
    let owner = conditional_target_owner("conditional-append",&[&source.directory,&target.directory]);
    let request = public_catalog_request(&source,&["absent"]);
    let (cold,entries) = public_catalog_finish(&store,&request,&owner);
    assert_eq!(cold[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,1);
    let appends = conditional_target_appends(&store);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&target.path).unwrap());
    target.append(" ");
    let current_stamp = SourceStamp::of(&std::fs::metadata(&target.path).unwrap());
    assert!(old_stamp != current_stamp);
    let (warm,entries) = public_catalog_finish(&store,&request,&owner);
    assert_eq!(warm[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,0);
    assert_eq!(conditional_target_appends(&store),appends);
    {
        let state = store.lock_state();
        let slot = state.catalogs.values().next().unwrap();
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled target stamp"); };
        let node = catalog.directories.values().find(|node| node.canonical == std::fs::canonicalize(&source.directory).unwrap()).unwrap();
        let edge = node.edges.iter().find(|edge| edge.name == OsStr::new("link.jsonl")).unwrap();
        let CatalogNameBinding::Conditional(target) = &edge.binding else { panic!("conditional target"); };
        assert!(target.binding.stamp == Some(current_stamp));
    }
    store.assert_conserved();
}

#[test]
fn public_catalog_conditional_changed_target_directory_replays_only_that_subtree() {
    let source = Source::new("");
    let target = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    std::fs::remove_file(&target.path).unwrap();
    for index in 0..32 { std::fs::write(source.directory.join(format!("ordinary-{index:02}.txt")),"").unwrap(); }
    std::fs::write(target.directory.join("old.txt"),"").unwrap();
    let link = source.directory.join("directory-link");
    std::os::unix::fs::symlink(&target.directory,&link).unwrap();
    let root_stamp = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
    let object_stamp = SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap());
    let store = store();
    let owner = conditional_target_owner("conditional-directory-change",&[&source.directory,&target.directory]);
    let negative = public_catalog_request(&source,&["absent"]);
    let (cold,entries) = public_catalog_finish(&store,&negative,&owner);
    assert_eq!(cold[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,34);
    let appended = conditional_target_appends(&store);
    std::fs::write(target.directory.join("wanted.jsonl"),"").unwrap();
    assert!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()) == root_stamp);
    assert!(SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap()) == object_stamp);
    let (changed,entries) = public_catalog_finish(&store,&negative,&owner);
    assert_eq!(changed[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,2,"changed target must not replay containing root/sibling names");
    assert_eq!(conditional_target_appends(&store)-appended,2);
    let selected = public_catalog_request(&source,&["wanted"]);
    let (found,entries) = public_catalog_finish(&store,&selected,&owner);
    assert_eq!(found[0]["status"].as_str(),Some("ok"));
    assert_eq!(Path::new(found[0]["path"].as_str().unwrap()),link.join("wanted.jsonl"));
    assert_eq!(entries,0);
    store.assert_conserved();
}

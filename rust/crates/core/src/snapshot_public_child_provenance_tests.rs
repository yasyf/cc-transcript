#[test]
fn public_catalog_child_provenance_survives_ancestor_aba_without_rewriting_the_private_observation() {
    for unknown in [false,true] {
        for absent in [false,true] {
            let layout = Source::new("");
            let parent = layout.directory.join("P");
            let saved = layout.directory.join("saved-P");
            let staged = layout.directory.join("staged-P");
            let directory = parent.join("A");
            let child = directory.join("a");
            let wanted = child.join("wanted.jsonl");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(&wanted,"").unwrap();
            std::fs::create_dir_all(staged.join("A")).unwrap();
            if !absent { std::fs::write(staged.join("A/a"),"").unwrap(); }
            let directory_stamp = SourceStamp::of(&std::fs::metadata(&directory).unwrap());
            let child_stamp = SourceStamp::of(&std::fs::metadata(&child).unwrap());
            let store = unknown_dtype_store();
            store.directory_force_unknown.store(unknown,Ordering::Relaxed);
            let swapped = Arc::new(AtomicUsize::new(0));
            let restored = Arc::new(AtomicUsize::new(0));
            let swap_parent = parent.clone();
            let swap_saved = saved.clone();
            let swap_staged = staged.clone();
            let swap_count = Arc::clone(&swapped);
            *store.location_metadata_hook.lock().unwrap() = Some(("canonical_prefilter",Arc::new(move || {
                std::fs::rename(&swap_parent,&swap_saved).unwrap();
                std::fs::rename(&swap_staged,&swap_parent).unwrap();
                swap_count.fetch_add(1,Ordering::Relaxed);
            })));
            let restore_parent = parent.clone();
            let restore_saved = saved.clone();
            let restore_staged = staged.clone();
            let restore_directory = directory.clone();
            let swap_count = Arc::clone(&swapped);
            let restore_count = Arc::clone(&restored);
            *store.canonical_error_hook.lock().unwrap() = Some(Arc::new(move |path| {
                if path == restore_directory && swap_count.load(Ordering::Relaxed) == 1
                    && restore_count.load(Ordering::Relaxed) == 0 {
                    std::fs::rename(&restore_parent,&restore_staged).unwrap();
                    std::fs::rename(&restore_saved,&restore_parent).unwrap();
                    restore_count.fetch_add(1,Ordering::Relaxed);
                }
                None
            }));
            let owner = context("child-provenance-aba");
            let mut request = public_catalog_request(&layout,&["wanted"]);
            request.insert("roots",json!([directory]));
            let (first,rows,entries) = unknown_dtype_collect(&store,store.request(&request,&owner,&Cancellation::default()),&owner);
            assert_eq!(first["status"].as_str(),Some("ok"),"unknown={unknown} absent={absent} {first:?}");
            assert_eq!(rows.len(),1);
            assert_eq!(rows[0]["status"].as_str(),Some("missing"),"private B observation remains immutable");
            assert_eq!(entries,1);
            assert_eq!(swapped.load(Ordering::Relaxed),1);
            assert_eq!(restored.load(Ordering::Relaxed),1);
            assert_eq!(store.directory_known_dir_names.load(Ordering::Relaxed),usize::from(!unknown));
            assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),usize::from(unknown),"known type mismatch rejects cheaply; UNKNOWN reads actual child metadata");
            assert_eq!(SourceStamp::of(&std::fs::metadata(&directory).unwrap()),directory_stamp);
            assert_eq!(SourceStamp::of(&std::fs::metadata(&child).unwrap()),child_stamp);
            {
                let state = store.lock_state();
                let slot = state.catalogs.values().next().unwrap();
                let cell = slot.cell.lock().unwrap();
                let CatalogCell::Idle(catalog) = &*cell else { panic!("settled first observation"); };
                let node = catalog.directories.get(&directory_stamp.identity).unwrap();
                assert_eq!(node.edges.len(),1);
                assert!(node.edges[0].binding.stamp.is_none(),"B metadata cannot become an A physical fact");
                assert!(node.edges[0].issue.is_none(),"B Missing cannot become an A physical error");
                assert_eq!(node.first_unproven,Some(0));
            }
            *store.canonical_error_hook.lock().unwrap() = None;
            let frozen = first.clone();
            let (next,found,_) = unknown_dtype_collect(&store,store.request(&request,&owner,&Cancellation::default()),&owner);
            assert_eq!(next["status"].as_str(),Some("ok"),"unknown={unknown} absent={absent} {next:?}");
            assert_eq!(found.len(),1);
            assert_eq!(found[0]["status"].as_str(),Some("ok"));
            assert_eq!(Path::new(found[0]["path"].as_str().unwrap()),wanted);
            assert_eq!(first,frozen);
            assert_eq!(rows[0]["status"].as_str(),Some("missing"));
            store.assert_conserved();
        }
    }
}

#[test]
fn public_catalog_child_provenance_caches_the_descriptor_revision_without_rewriting_the_private_observation() {
    let source = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let path = source.directory.join("ordinary.txt");
    std::fs::write(&path,"old").unwrap();
    let old = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    let root = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
    let store = unknown_dtype_store();
    store.directory_force_unknown.store(false,Ordering::Relaxed);
    let changed = path.clone();
    let called = Arc::new(AtomicUsize::new(0));
    let callback = Arc::clone(&called);
    *store.location_metadata_hook.lock().unwrap() = Some(("prefilter",Arc::new(move || {
        std::fs::write(&changed,"a newer revision of the same inode").unwrap();
        callback.fetch_add(1,Ordering::Relaxed);
    })));
    let owner = context("child-descriptor-revision");
    let request = public_catalog_request(&source,&["absent"]);
    let (_,rows,entries) = unknown_dtype_collect(&store,store.request(&request,&owner,&Cancellation::default()),&owner);
    assert_eq!(called.load(Ordering::Relaxed),1);
    assert_eq!(rows[0]["status"].as_str(),Some("missing"));
    assert_eq!(entries,1);
    let current = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    assert_eq!(current.identity,old.identity);
    assert_ne!(current,old);
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()),root);
    let state = store.lock_state();
    let slot = state.catalogs.values().next().unwrap();
    let cell = slot.cell.lock().unwrap();
    let CatalogCell::Idle(catalog) = &*cell else { panic!("settled descriptor revision"); };
    let node = catalog.directories.get(&root.identity).unwrap();
    assert_eq!(node.edges[0].binding.stamp,Some(current));
    assert_ne!(node.edges[0].binding.stamp,Some(old));
    assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),1);
    drop(cell);
    drop(state);
    store.assert_conserved();
}

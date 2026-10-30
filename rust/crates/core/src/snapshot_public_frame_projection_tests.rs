#[test]
fn public_catalog_conditional_target_identity_replacement_preserves_a_paged_private_name_view() {
    let source = Source::new("");
    let targets = Source::new("");
    std::fs::remove_file(&source.path).unwrap();
    let mut root = source.directory.clone();
    while root.as_os_str().len() < 740 {
        let length = (740-root.as_os_str().len()-1).min(180);
        assert!(length > 0);
        root = root.join("d".repeat(length));
        std::fs::create_dir(&root).unwrap();
    }
    let target = targets.directory.join("T");
    let staged = targets.directory.join("newT");
    let saved = targets.directory.join("savedT");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&staged).unwrap();
    let old_ids: Vec<_> = (0..900).map(|index| format!("old{index:04}-{}","x".repeat(232))).collect();
    for id in &old_ids { std::fs::write(target.join(format!("{id}.jsonl")),"").unwrap(); }
    for index in 0..1400 { std::fs::write(staged.join(format!("new{index:04}-{}.jsonl","x".repeat(232))),"").unwrap(); }
    let new_names: Vec<_> = std::fs::read_dir(&staged).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    let new_ordinal = new_names.len()-1;
    let new_id = Path::new(new_names.last().unwrap()).file_stem().unwrap().to_str().unwrap().to_owned();
    assert_eq!(new_ordinal,1399);
    let link = root.join("link");
    std::os::unix::fs::symlink(&target,&link).unwrap();
    let root_stamp = SourceStamp::of(&std::fs::metadata(&root).unwrap());
    let link_stamp = SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap());
    let old_stamp = SourceStamp::of(&std::fs::metadata(&target).unwrap());
    let store = NativeStore::new(&json!({"max_events_per_step":128,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    let owner = context("private-projection-old");
    let foreign = context("private-projection-foreign");
    let mut prime = public_catalog_request(&source,&["absent"]);
    prime.insert("roots",json!([root]));
    let (missing,_) = public_catalog_finish(&store,&prime,&owner);
    assert_eq!(missing[0]["status"].as_str(),Some("missing"));
    let slot = {
        let state = store.lock_state();
        let slot = state.catalogs.values().next().cloned().unwrap();
        slot
    };
    let (old_generation,old_token) = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("primed catalog"); };
        let node = catalog.directories.get(&old_stamp.identity).unwrap();
        (node.generation,Arc::downgrade(&node.lease))
    };
    let mut wanted: Vec<_> = old_ids.iter().map(String::as_str).collect();
    wanted.push(&new_id);
    let mut request = public_catalog_request(&source,&wanted);
    request.insert("roots",json!([root]));
    request["limits"].insert("max_output_bytes",json!(8*1024*1024));
    let first = store.request(&request,&owner,&Cancellation::default());
    assert_eq!(first["status"].as_str(),Some("incomplete"),"{first:?}");
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(),Some(0));
    let emitted = first["data"]["sessions"].as_array().unwrap();
    assert!(!emitted.is_empty() && emitted.len() < old_ids.len());
    let cursor = first["cursor"].as_str().expect("real MAX_DATA_BYTES output page").to_owned();
    let (floor,old_projection,old_candidates) = {
        let state = store.lock_state();
        let query = &state.locates.get(&cursor).unwrap().query;
        assert!(!query.observation_started && !query.used_name_stream);
        let frame = query.frames.iter().find(|frame| frame.identity == old_stamp.identity).expect("leased cached old target frame");
        assert_eq!(frame.generation,old_generation);
        assert_eq!(frame.raw_parent,link);
        let floor = *query.floor.as_ref().unwrap().last().unwrap();
        assert!(floor < old_ids.len() && new_ordinal > floor);
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("parked catalog"); };
        (floor,catalog.generation_version,catalog.candidate_version)
    };
    super::ledger_tests::assert_public_frame_projection_ledger(&store);
    std::fs::rename(&target,&saved).unwrap();
    std::fs::rename(&staged,&target).unwrap();
    assert!(SourceStamp::of(&std::fs::metadata(&root).unwrap()) == root_stamp);
    assert!(SourceStamp::of(&std::fs::symlink_metadata(&link).unwrap()) == link_stamp);
    let new_stamp = SourceStamp::of(&std::fs::metadata(&target).unwrap());
    assert!(new_stamp.identity != old_stamp.identity);
    let (foreign_missing,_) = public_catalog_finish(&store,&prime,&foreign);
    assert_eq!(foreign_missing[0]["status"].as_str(),Some("missing"));
    {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("foreign target refresh"); };
        assert!(catalog.retired_generations.contains_key(&old_generation));
        assert!(!catalog.directories.contains_key(&old_stamp.identity));
        let new_node = catalog.directories.get(&new_stamp.identity).unwrap();
        assert_eq!(new_node.edges[new_ordinal].name,new_names[new_ordinal]);
        assert!(catalog.candidates.get(&new_id).is_some_and(|candidates| candidates.iter().any(|candidate| candidate.directory == new_stamp.identity && candidate.edge == new_ordinal)));
        assert!(catalog.candidate_version > old_candidates);
        eprintln!("paged_private_old_floor={floor} new_ordinal={new_ordinal} generation_before={old_projection} generation_after={}",catalog.generation_version);
    }
    assert!(old_token.upgrade().is_some(),"foreign refresh must retain the old query lease");
    super::ledger_tests::assert_public_frame_projection_ledger(&store);
    let (last,rows,_) = public_observation_collect(&store,public_observation_resume(&store,&cursor,&owner),&owner);
    assert_eq!(last["status"].as_str(),Some("ok"),"{last:?}");
    let new_row = rows.iter().find(|row| row["session_id"].as_str() == Some(new_id.as_str())).expect("new-only wanted ID gets a final private result");
    assert_eq!(new_row["status"].as_str(),Some("missing"),"current index must not replace the old leased NAME view");
    assert!(old_token.upgrade().is_none(),"completion must reclaim the retired generation before any followup request/audit");
    {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("completed original cursor"); };
        assert!(!catalog.retired_generations.contains_key(&old_generation));
        assert!(catalog.retirement_queue.is_empty());
        assert_eq!(catalog.retired_bytes,0);
    }
    super::ledger_tests::assert_public_frame_projection_ledger(&store);
    let mut fresh = public_catalog_request(&source,&[&new_id]);
    fresh.insert("roots",json!([root]));
    let (found,_) = public_catalog_finish(&store,&fresh,&foreign);
    assert_eq!(found[0]["status"].as_str(),Some("ok"));
    assert_eq!(Path::new(found[0]["path"].as_str().unwrap()),link.join(format!("{new_id}.jsonl")));
    super::ledger_tests::assert_public_frame_projection_ledger(&store);
}

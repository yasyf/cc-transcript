fn public_observation_ledger_slot(store: &NativeStore) -> Arc<CatalogSlot> {
    let state = store.lock_state();
    assert_eq!(state.catalogs.len(), 1);
    Arc::clone(state.catalogs.values().next().unwrap())
}

fn public_observation_ledger_audit(store: &NativeStore) {
    assert_eq!(ledger(store), audited(store));
    store.assert_conserved();
}

#[test]
fn location_observation_retired_generation_is_charged_until_cancel_or_release_of_its_last_query_pin() {
    for release in [false, true] {
        let source = LedgerSource::new("");
        std::fs::remove_file(&source.path).unwrap();
        for index in 0..4 { source.file(&format!("ordinary-{index}.txt"), ""); }
        let store = public_review_store(1);
        let owner = context_for("public-observation-retire-a", false);
        let foreign = context_for("public-observation-retire-b", false);
        let request = public_review_request(&source, &["absent"]);
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
        let cursor = first["cursor"].as_str().expect("pinned old-generation cursor").to_owned();
        let slot = public_observation_ledger_slot(&store);
        let (identity, old_generation, old_token, before_work) = {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("settled catalog"); };
            let directory = catalog.directories.values().next().unwrap();
            assert_eq!(Arc::strong_count(&directory.lease), 2);
            (directory.observed.stamp.unwrap().identity, directory.generation, Arc::downgrade(&directory.lease), catalog.physical_append_work)
        };
        public_observation_ledger_audit(&store);
        source.file("new-name.txt", "");
        let (_, rows) = public_review_finish(&store, &request, &foreign);
        assert_eq!(rows[0]["status"].as_str(), Some("missing"));
        {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("restored catalog"); };
            assert_ne!(catalog.directories.get(&identity).unwrap().generation, old_generation);
            let retired = catalog.retired_generations.get(&old_generation).expect("old query keeps its generation");
            assert_eq!(Arc::strong_count(&retired.lease), 2);
            assert_eq!(catalog.retired_generations.len(), 1);
            assert_eq!(catalog.retired_bytes, retired.owned_bytes());
            assert!(catalog.retirement_queue.contains(&old_generation));
            assert_eq!(catalog.physical_append_work - before_work, 5);
        }
        public_observation_ledger_audit(&store);
        let retired_charge = ledger(&store)[TOTAL];
        let stopped = if release {
            store.request(&json!({"schema":SCHEMA,"id":"public-observation-release","operation":"release","kind":"cursor","owner_epoch":null,"token":cursor}), &owner, &Cancellation::default())
        } else {
            let cancel = Cancellation::default();
            cancel.cancel();
            store.request(&json!({"schema":SCHEMA,"id":"public-observation-cancel","operation":"resume","cursor":cursor}), &owner, &cancel)
        };
        assert_eq!(stopped["status"].as_str(), Some(if release { "ok" } else { "cancelled" }), "{stopped:?}");
        assert!(stopped["cursor"].is_null());
        public_observation_ledger_audit(&store);
        let (_, rows) = public_review_finish(&store, &request, &foreign);
        assert_eq!(rows[0]["status"].as_str(), Some("missing"));
        {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("settled current generation"); };
            assert!(catalog.retired_generations.is_empty());
            assert!(catalog.retirement_queue.is_empty());
            assert_eq!(catalog.retired_bytes, 0);
        }
        assert!(old_token.upgrade().is_none(), "the last-pin release must reclaim the old payload and reader");
        assert!(store.lock_state().locates.is_empty());
        assert!(ledger(&store)[TOTAL] < retired_charge);
        public_observation_ledger_audit(&store);
    }
}

#[test]
fn location_observation_store_drop_releases_both_live_and_retired_generation_tokens() {
    let source = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..4 { source.file(&format!("ordinary-{index}.txt"), ""); }
    let store = public_review_store(1);
    let owner = context_for("public-observation-drop-a", false);
    let foreign = context_for("public-observation-drop-b", false);
    let request = public_review_request(&source, &["absent"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    assert!(first["cursor"].as_str().is_some());
    source.file("new-name.txt", "");
    public_review_finish(&store, &request, &foreign);
    let slot = public_observation_ledger_slot(&store);
    let (live, retired) = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled catalog"); };
        assert_eq!(catalog.retired_generations.len(), 1);
        (Arc::downgrade(&catalog.directories.values().next().unwrap().lease), Arc::downgrade(&catalog.retired_generations.values().next().unwrap().lease))
    };
    let owner_token = Arc::downgrade(&slot);
    public_observation_ledger_audit(&store);
    drop(slot);
    drop(store);
    assert!(owner_token.upgrade().is_none());
    assert!(live.upgrade().is_none());
    assert!(retired.upgrade().is_none());
}

#[test]
fn location_observation_last_pin_release_while_owner_busy_reclaims_at_settlement() {
    let source = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..4 { source.file(&format!("ordinary-{index}.txt"), ""); }
    let store = Arc::new(public_review_store(1));
    let owner = context_for("public-observation-busy-a", false);
    let foreign = context_for("public-observation-busy-b", false);
    let request = public_review_request(&source, &["absent"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    let cursor = first["cursor"].as_str().expect("pinned original query").to_owned();
    let slot = public_observation_ledger_slot(&store);
    let old_token = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled original generation"); };
        Arc::downgrade(&catalog.directories.values().next().unwrap().lease)
    };
    source.file("new-name.txt", "");
    let weak_store = Arc::downgrade(&store);
    let release_owner = owner.clone();
    let called = Arc::new(AtomicUsize::new(0));
    let called_hook = Arc::clone(&called);
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || {
        let store = weak_store.upgrade().unwrap();
        let slot = public_observation_ledger_slot(&store);
        assert!(matches!(*slot.cell.lock().unwrap(), CatalogCell::Busy));
        let released = store.request(&json!({"schema":SCHEMA,"id":"release-old-while-busy","operation":"release","kind":"cursor","owner_epoch":null,"token":cursor}), &release_owner, &Cancellation::default());
        assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
        called_hook.fetch_add(1, Ordering::Relaxed);
    })));
    let rebuilding = store.request(&request, &foreign, &Cancellation::default());
    assert_eq!(called.load(Ordering::Relaxed), 1);
    assert_eq!(rebuilding["status"].as_str(), Some("incomplete"), "{rebuilding:?}");
    assert!(rebuilding["cursor"].as_str().is_some());
    {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("new owner must settle"); };
        assert!(catalog.retired_generations.is_empty());
        assert!(catalog.retirement_queue.is_empty());
        assert_eq!(catalog.retired_bytes, 0);
    }
    assert!(old_token.upgrade().is_none());
    public_observation_ledger_audit(&store);
    let released = store.request(&json!({"schema":SCHEMA,"id":"release-busy-rebuilder","operation":"release","kind":"cursor","owner_epoch":null,"token":rebuilding["cursor"]}), &foreign, &Cancellation::default());
    assert_eq!(released["status"].as_str(), Some("ok"));
    public_observation_ledger_audit(&store);
}

fn public_observation_retirement_admission_attempt(headroom: usize, verify_recovery: bool) -> bool {
    let source = LedgerSource::new("");
    std::fs::remove_file(&source.path).unwrap();
    for index in 0..4 { source.file(&format!("ordinary-{index}.txt"), ""); }
    let store = public_review_store(1);
    let owner = context_for("public-observation-fit-a", false);
    let foreign = context_for("public-observation-fit-b", false);
    let request = public_review_request(&source, &["absent"]);
    let first = store.request(&request, &owner, &Cancellation::default());
    let cursor = first["cursor"].as_str().expect("pinned original generation").to_owned();
    let slot = public_observation_ledger_slot(&store);
    let (identity, generation, token) = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("settled catalog"); };
        let directory = catalog.directories.values().next().unwrap();
        (directory.observed.stamp.unwrap().identity, directory.generation, Arc::downgrade(&directory.lease))
    };
    source.file("new-name.txt", "");
    let filler = fill_to(&store, &foreign, headroom);
    let response = store.request(&request, &foreign, &Cancellation::default());
    let admitted = response["status"].as_str() == Some("incomplete") && response["cursor"].as_str().is_some();
    assert!(admitted || response["status"].as_str() == Some("retained_limit"), "{response:?}");
    {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("admission must restore the owned partial catalog"); };
        let original = catalog.directories.get(&identity).filter(|node| node.generation == generation)
            .or_else(|| catalog.retired_generations.get(&generation)).expect("refusal must preserve the pinned generation");
        assert_eq!(Arc::strong_count(&original.lease), 2);
    }
    public_observation_ledger_audit(&store);
    drop(filler);
    if verify_recovery {
        if let Some(cursor) = response["cursor"].as_str() {
            let cancel = Cancellation::default();
            cancel.cancel();
            let stopped = store.request(&json!({"schema":SCHEMA,"id":"stop-fit-probe","operation":"resume","cursor":cursor}), &foreign, &cancel);
            assert_eq!(stopped["status"].as_str(), Some("cancelled"));
        }
        let (_, rows) = public_review_finish(&store, &request, &foreign);
        assert_eq!(rows[0]["status"].as_str(), Some("missing"));
        let stopped = store.request(&json!({"schema":SCHEMA,"id":"release-fit-original","operation":"release","kind":"cursor","owner_epoch":null,"token":cursor}), &owner, &Cancellation::default());
        assert_eq!(stopped["status"].as_str(), Some("ok"), "{stopped:?}");
        public_review_finish(&store, &request, &foreign);
        assert!(token.upgrade().is_none());
        public_observation_ledger_audit(&store);
    }
    admitted
}

#[test]
fn location_observation_generation_retirement_and_new_lease_obey_exact_admission() {
    let exact = exact_fit(&|headroom| public_observation_retirement_admission_attempt(headroom, false));
    assert!(exact > 0);
    assert!(!public_observation_retirement_admission_attempt(exact - 1, true));
    assert!(public_observation_retirement_admission_attempt(exact, true));
}

fn public_observation_raw_name_admission_attempt(headroom: usize, verify_recovery: bool) -> bool {
    let source = LedgerSource::new("");
    let store: &'static NativeStore = Box::leak(Box::new(public_review_store(1)));
    let owner = context_for("public-observation-pending-name", false);
    let request = public_review_request(&source, &["s"]);
    let held: Arc<Mutex<Option<ProjectionReservation<'static>>>> = Arc::new(Mutex::new(None));
    let pinned: Arc<Mutex<Option<Arc<CatalogSlot>>>> = Arc::new(Mutex::new(None));
    let called = Arc::new(AtomicUsize::new(0));
    let held_hook = Arc::clone(&held);
    let pinned_hook = Arc::clone(&pinned);
    let called_hook = Arc::clone(&called);
    let hook_owner = owner.clone();
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || {
        called_hook.fetch_add(1, Ordering::Relaxed);
        *pinned_hook.lock().unwrap() = Some(public_observation_ledger_slot(store));
        let available = cap_of(store, &hook_owner) - ledger(store)[TOTAL];
        assert!(headroom <= available);
        *held_hook.lock().unwrap() = Some(store.reserve_projection(&hook_owner, available - headroom).unwrap());
    })));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(called.load(Ordering::Relaxed), 1);
    assert!(matches!(response["status"].as_str(), Some("ok" | "incomplete" | "retained_limit")), "{response:?}");
    let slot = pinned.lock().unwrap().as_ref().cloned().unwrap();
    let committed = {
        let cell = slot.cell.lock().unwrap();
        let CatalogCell::Idle(catalog) = &*cell else { panic!("raw-name refusal must restore the catalog"); };
        let node = catalog.directories.values().next().unwrap();
        assert_eq!(catalog.physical_append_work, 1);
        if let Some((name, _)) = &node.pending_name {
            assert_eq!(name, OsStr::new("s.jsonl"));
            assert!(node.edges.is_empty());
            false
        } else {
            assert_eq!(node.edges.len(), 1);
            assert_eq!(node.edges[0].name, OsStr::new("s.jsonl"));
            true
        }
    };
    public_observation_ledger_audit(store);
    drop(held.lock().unwrap().take());
    if verify_recovery {
        if let Some(cursor) = response["cursor"].as_str() {
            let released = store.request(&json!({"schema":SCHEMA,"id":"release-raw-name-probe","operation":"release","kind":"cursor","owner_epoch":null,"token":cursor}), &owner, &Cancellation::default());
            assert_eq!(released["status"].as_str(), Some("ok"));
        }
        let (_, rows) = public_review_finish(store, &request, &owner);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"].as_str(), Some("ok"));
        assert_eq!(Path::new(rows[0]["path"].as_str().unwrap()), source.path);
        {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("restored raw-name progress"); };
            assert_eq!(catalog.physical_append_work, 1, "retry must reuse the consumed pending name");
            assert!(catalog.directories.values().all(|node| node.pending_name.is_none()));
        }
        public_observation_ledger_audit(store);
    }
    drop(pinned.lock().unwrap().take());
    committed
}

#[test]
fn location_observation_raw_name_commit_refusal_preserves_owned_progress_at_exact_fit() {
    let exact = exact_fit(&|headroom| public_observation_raw_name_admission_attempt(headroom, false));
    assert!(exact > 0);
    assert!(!public_observation_raw_name_admission_attempt(exact - 1, true));
    assert!(public_observation_raw_name_admission_attempt(exact, true));
}

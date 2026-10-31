fn catalog_counted_request(store: &NativeStore, request: &Value, owner: &Value, cancel: &Cancellation) -> (Value, u64) {
    let mut response = store.request(request, owner, cancel);
    let mut entries = response["usage"]["discovery_entries_examined"].as_u64().unwrap();
    for _ in 0..4096 {
        if response["status"].as_str() != Some("incomplete") { return (response, entries); }
        let Some(cursor) = response["cursor"].as_str() else { return (response, entries); };
        response = store.request(&json!({"schema":SCHEMA,"id":"catalog-resume","operation":"resume","cursor":cursor}), owner, cancel);
        entries += response["usage"]["discovery_entries_examined"].as_u64().unwrap();
    }
    panic!("catalog request did not settle");
}

fn catalog_missing_request(source: &Source, root: &Value, ids: &[String]) -> Value {
    let ids: Vec<_> = ids.iter().map(String::as_str).collect();
    let mut request = registered_prepare(source, root, &ids);
    request["limits"].insert("max_items", json!(65_536));
    request["limits"].insert("max_sources", json!(4096));
    request["limits"].insert("max_discovery_entries", json!(50_000));
    request.insert("deadline_unix_ms", json!(now_ms() + 750));
    request
}

fn catalog_finish_missing(store: &NativeStore, request: &Value, owner: &Value, entry_limit: u64) -> (Value, u64) {
    let mut entries = 0;
    for _ in 0..20 {
        let mut request = request.clone();
        request.insert("deadline_unix_ms", json!(now_ms() + 750));
        let (response, read) = catalog_counted_request(store, &request, owner, &Cancellation::default());
        entries += read;
        assert!(entries <= entry_limit, "directory entries={entries} limit={entry_limit} status={:?}", response["status"].as_str());
        if response["status"].as_str() == Some("deadline") { continue; }
        assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
        assert_eq!(response["reason"].as_str(), Some("registered membership is incomplete"), "{response:?}");
        assert!(response["cursor"].is_null(), "{response:?}");
        return (response, entries);
    }
    panic!("owned catalog made no completed progress across private deadlines");
}

#[test]
fn registered_catalog_reuses_cancelled_indexing_for_a_4587_id_roster() {
    let source = Source::new(&format!("{}\n", user("root")));
    for index in 0..4587 {
        std::fs::write(source.directory.join(format!("unrelated-{index:04}.txt")), "").unwrap();
    }
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
    let owner = context("catalog-roster");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let ids: Vec<_> = (0..4587).map(|index| format!("missing-{index:04}")).collect();
    let request = catalog_missing_request(&source, &root, &ids);
    let cancel = Cancellation::default();
    let observed = Arc::new(AtomicUsize::new(0));
    let observed_hook = Arc::clone(&observed);
    let cancel_hook = cancel.clone();
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || {
        if observed_hook.fetch_add(1, Ordering::Relaxed) == 0 { cancel_hook.cancel(); }
    })));
    let (cancelled, first_entries) = catalog_counted_request(&store, &request, &owner, &cancel);
    *store.location_metadata_hook.lock().unwrap() = None;
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"), "{cancelled:?}");
    assert_eq!(observed.load(Ordering::Relaxed), 1);
    store.assert_conserved();
    let (_, resumed_entries) = catalog_finish_missing(&store, &request, &owner, 4588 - first_entries);
    assert_eq!(first_entries + resumed_entries, 4588, "owned indexing re-enumerated its prefix");
    let (_, warm_entries) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(warm_entries, 0, "unchanged missing-ID roster re-enumerated the corpus");
    store.assert_conserved();
}

#[test]
fn registered_catalog_reenumerates_only_the_changed_directory() {
    let source = Source::new(&format!("{}\n", user("root")));
    for name in ["left", "right"] {
        let directory = source.directory.join(name);
        std::fs::create_dir(&directory).unwrap();
        for index in 0..32 { std::fs::write(directory.join(format!("unrelated-{index:02}.txt")), "").unwrap(); }
    }
    let store = store();
    let owner = context("catalog-directory-change");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let ids: Vec<_> = (0..4587).map(|index| format!("missing-{index:04}")).collect();
    let request = catalog_missing_request(&source, &root, &ids);
    let (_, cold) = catalog_finish_missing(&store, &request, &owner, 67);
    assert_eq!(cold, 67);
    let (_, warm) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(warm, 0);
    std::fs::write(source.directory.join("left").join("new.txt"), "").unwrap();
    let (_, changed) = catalog_finish_missing(&store, &request, &owner, 33);
    assert_eq!(changed, 33, "an unchanged sibling directory was re-enumerated");
    let (_, settled) = catalog_finish_missing(&store, &request, &owner, 0);
    assert_eq!(settled, 0);
    store.assert_conserved();
}

#[test]
fn registered_catalog_deep_directory_metadata_work_is_linear() {
    for depth in [32, 64, 128] {
        let source = Source::new(&format!("{}\n", user("root")));
        let mut directory = source.directory.clone();
        for _ in 0..depth {
            directory.push("d");
            std::fs::create_dir(&directory).unwrap();
        }
        std::fs::write(directory.join("unrelated.txt"), "").unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":1,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("catalog-deep-work");
        let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let request = catalog_missing_request(&source, &root, &["missing".to_owned()]);
        let before = store.location_canonicalizations.load(Ordering::Relaxed);
        let (_, entries) = catalog_finish_missing(&store, &request, &owner, (depth + 2) as u64);
        let resolutions = store.location_canonicalizations.load(Ordering::Relaxed) - before;
        assert_eq!(entries, (depth + 2) as u64);
        assert!(resolutions <= 32 * depth + 1024, "depth={depth} location_canonicalizations={resolutions}");
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_cursor_charge_does_not_walk_its_immutable_request() {
    for count in [33, 129, 513] {
        let source = Source::new(&format!("{}\n", user("root")));
        std::fs::write(source.directory.join("unrelated.txt"), "").unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":1,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("catalog-readonly-charge");
        let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let ids: Vec<_> = (0..count).map(|index| format!("missing-{index}")).collect();
        let request = catalog_missing_request(&source, &root, &ids);
        let response = store.request(&request, &owner, &Cancellation::default());
        let cursor = response["cursor"].as_str().expect("catalog private cursor");
        let state = store.lock_state();
        let build = state.registered_builds.get(cursor).expect("registered build");
        let audit = NativeStore::audit_registered_build_bytes(&cursor.to_owned(), build) - cursor.len();
        crate::snapshot_memory::DOM_WORK.with(|work| work.set(0));
        for _ in 0..64 { assert_eq!(build.charge(), audit); }
        let work = crate::snapshot_memory::DOM_WORK.with(std::cell::Cell::get);
        assert_eq!(work, 0, "readonly charge walked immutable roster count={count}");
        drop(state);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_remembers_a_valid_winner_across_root_reordering() {
    let source = Source::new(&format!("{}\n", user("root")));
    let left = source.directory.join("left");
    let right = source.directory.join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    std::fs::write(left.join("wanted.jsonl"), format!("{}\n{}\n", user("left"), tool_use_line("Read"))).unwrap();
    std::fs::write(right.join("wanted.jsonl"), format!("{}\n{}\n", user("right"), tool_use_line("Grep"))).unwrap();
    let store = store();
    let owner = context("catalog-winner");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = registered_prepare(&source, &root, &["wanted"]);
    request.insert("roots", json!([right.to_string_lossy().as_ref(), left.to_string_lossy().as_ref()]));
    let first = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert!(graph_has_tool(&store, &source, &owner, &first, "Read"));
    assert!(!graph_has_tool(&store, &source, &owner, &first, "Grep"));
    request.insert("roots", json!([left.to_string_lossy().as_ref(), right.to_string_lossy().as_ref()]));
    let reordered = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert!(graph_has_tool(&store, &source, &owner, &reordered, "Read"));
    assert!(!graph_has_tool(&store, &source, &owner, &reordered, "Grep"));
    store.assert_conserved();
}

struct CatalogOwnerRelease(Arc<(Mutex<bool>, Condvar)>);

impl Drop for CatalogOwnerRelease {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

#[test]
fn registered_catalog_waiter_cancels_without_releasing_the_busy_owner() {
    let source = Source::new(&format!("{}\n", user("root")));
    let store = Arc::new(store());
    let owner = context("catalog-waiter");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let request = registered_prepare(&source, &root, &["missing"]);
    let (entered, observed_entry) = std::sync::mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_drop = CatalogOwnerRelease(Arc::clone(&release));
    let release_hook = Arc::clone(&release);
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || {
        entered.send(()).unwrap();
        let mut released = release_hook.0.lock().unwrap();
        while !*released { released = release_hook.1.wait(released).unwrap(); }
    })));
    let first_store = Arc::clone(&store);
    let first_request = request.clone();
    let first_owner = owner.clone();
    let first = std::thread::spawn(move || catalog_counted_request(&first_store, &first_request, &first_owner, &Cancellation::default()).0);
    observed_entry.recv_timeout(Duration::from_secs(5)).unwrap();
    let (waiting, observed_wait) = std::sync::mpsc::channel();
    *store.catalog_wait_hook.lock().unwrap() = Some(Arc::new(move || { waiting.send(()).unwrap(); }));
    let second_store = Arc::clone(&store);
    let cancel = Cancellation::default();
    let second_cancel = cancel.clone();
    let (settled, observed_settlement) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        settled.send(second_store.request(&request, &owner, &second_cancel)).unwrap();
    });
    observed_wait.recv_timeout(Duration::from_secs(5)).unwrap();
    cancel.cancel();
    let cancelled = observed_settlement.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"), "{cancelled:?}");
    *store.catalog_wait_hook.lock().unwrap() = None;
    drop(release_on_drop);
    second.join().unwrap();
    let completed = first.join().unwrap();
    assert_eq!(completed["status"].as_str(), Some("incomplete"), "{completed:?}");
    *store.location_metadata_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn registered_catalog_remembers_an_earlier_chunk_before_a_later_error() {
    let source = Source::new(&format!("{}\n", user("root")));
    let outside = Source::new("");
    let good = source.directory.join("good");
    let bad = source.directory.join("bad");
    std::fs::create_dir(&good).unwrap();
    std::fs::create_dir(&bad).unwrap();
    let wanted = good.join("wanted.jsonl");
    std::fs::write(&wanted, format!("{}\n", user("wanted"))).unwrap();
    std::os::unix::fs::symlink(&outside.path, bad.join("unrelated.txt")).unwrap();
    let store = store();
    let mut owner = context("catalog-earlier-chunk");
    owner.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory.to_string_lossy().as_ref()],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = registered_prepare(&source, &root, &["wanted", "missing"]);
    request.insert("roots", json!([bad.to_string_lossy().as_ref(), good.to_string_lossy().as_ref()]));
    request["limits"].insert("max_items", json!(1));
    let denied = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(denied["status"].as_str(), Some("permission_denied"), "{denied:?}");
    {
        let state = store.lock_state();
        assert_eq!(state.locations.get("wanted").map(|location| &location.path), Some(&wanted));
        let mut unresolved = false;
        for slot in state.catalogs.values() {
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("settled catalog"); };
            for node in catalog.directories.values() {
                assert!(node.observed.issue.as_ref().is_none_or(|(status, _)| *status != Status::PermissionDenied));
                for edge in &node.edges {
                    unresolved |= edge.binding.is_none();
                    assert!(edge.binding.as_ref().and_then(|binding| binding.issue.as_ref()).is_none_or(|(status, _)| *status != Status::PermissionDenied));
                }
            }
        }
        assert!(unresolved, "scope refusal must retain only a NAME barrier");
    }
    store.assert_conserved();
}

#[test]
fn registered_catalog_key_admits_escaped_capacity_before_encoding() {
    let source = Source::new("");
    let directory = source.directory.join("\u{01}".repeat(240));
    std::fs::create_dir(&directory).unwrap();
    let roots: Vec<_> = (0..64).map(|_| directory.to_str().unwrap()).collect();
    let request = json!({"roots":roots,"thread_ids":["missing"]});
    for background in [false, true] {
        let mut owner = context("catalog-key-capacity");
        if background { owner.insert("work_class", json!("background")); }
        let control = store();
        let (key, reservation) = control.catalog_key(request["roots"].as_array().unwrap(), &owner).unwrap();
        let bound = reservation.bytes;
        let binding = json!({"authority":owner["authority"],"roots":request["roots"]});
        let expected = crate::ids::canonical_json(&binding).unwrap();
        let old_bound = LOCATE_USER_PATH_SLOTS + 2 * value_bytes(&owner["authority"])
            + 2 * value_bytes(&request["roots"]) + 1024;
        assert!(expected.len() > old_bound);
        assert_eq!(key, format!("{:x}", Sha256::digest(expected.as_bytes())));
        drop(reservation);
        control.assert_conserved();
        for (headroom, expected_encodes) in [(bound - 1, 0), (bound, 1)] {
            let store = store();
            let encoded = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&encoded);
            *store.decode_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                if stage == "catalog_key" { observed.fetch_add(1, Ordering::Relaxed); }
            }));
            let cap = store.config.retained - if background { store.config.hook_bytes } else { 0 };
            let baseline = store.retained_accounted_bytes();
            let filler = store.reserve_projection(&owner, cap - baseline - headroom).unwrap();
            let before = store.retained_accounted_bytes();
            let mut limits = WorkLimits {
                max_read_bytes: 8192, max_source_read_bytes: 8192, max_events: 128,
                max_items: 1024, max_output_bytes: 8192, max_discovery_entries: 128,
                max_sources: 128, deadline_unix_ms: now_ms() + 30_000,
            };
            let outcome = store.catalog_access(CatalogInput { ids: request["thread_ids"].as_array().unwrap(), roots: request["roots"].as_array().unwrap() }, &owner, &Cancellation::default(),
                &mut limits, &mut [0;20], CatalogMode::Locate);
            assert!(matches!(outcome, Err(error) if error.status == Status::RetainedLimit));
            assert_eq!(encoded.load(Ordering::Relaxed), expected_encodes);
            assert_eq!(store.retained_accounted_bytes(), before);
            assert!(store.lock_state().catalogs.is_empty());
            drop(filler);
            store.assert_conserved();
        }
    }
}

fn catalog_finish_positive(store: &NativeStore, request: &Value, owner: &Value, entry_limit: u64) -> (Value, u64) {
    let mut entries = 0;
    for attempt in 0..20 {
        let mut request = request.clone();
        request.insert("deadline_unix_ms", json!(now_ms() + 750));
        let comparisons = store.catalog_id_comparisons.load(Ordering::Relaxed);
        let canonicalizations = store.location_canonicalizations.load(Ordering::Relaxed);
        let member_checks = store.membership_metadata_checks.load(Ordering::Relaxed);
        let (response, read) = catalog_counted_request(store, &request, owner, &Cancellation::default());
        eprintln!("catalog_positive attempt={attempt} status={:?} reason={:?} entries={read} id_comparisons={} canonicalizations={} member_checks={}",
            response["status"].as_str(), response["reason"].as_str(),
            store.catalog_id_comparisons.load(Ordering::Relaxed) - comparisons,
            store.location_canonicalizations.load(Ordering::Relaxed) - canonicalizations,
            store.membership_metadata_checks.load(Ordering::Relaxed) - member_checks);
        entries += read;
        assert!(entries <= entry_limit, "positive entries={entries} limit={entry_limit} status={:?}", response["status"].as_str());
        if response["status"].as_str() == Some("deadline") { continue; }
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert!(response["cursor"].is_null(), "{response:?}");
        return (response, entries);
    }
    panic!("positive registered membership did not finish within original private deadlines");
}

#[test]
fn registered_catalog_materializes_a_4587_source_roster_and_observes_one_append() {
    let source = Source::new(&format!("{}\n", user("root")));
    let ids: Vec<_> = (0..4587).map(|index| format!("thread-{index:04}")).collect();
    for id in &ids {
        std::fs::write(source.directory.join(format!("{id}.jsonl")), format!("{}\n", user(id))).unwrap();
    }
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
    let owner = context("catalog-positive-roster");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &ids);
    request["limits"].insert("max_sources", json!(8192));
    let cancel = Cancellation::default();
    let cancel_hook = cancel.clone();
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new(move || cancel_hook.cancel())));
    let (cancelled, interrupted_entries) = catalog_counted_request(&store, &request, &owner, &cancel);
    *store.location_metadata_hook.lock().unwrap() = None;
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"), "{cancelled:?}");
    assert_eq!(interrupted_entries, 1);
    store.assert_conserved();
    let (_, cold_entries) = catalog_finish_positive(&store, &request, &owner, 4588 - interrupted_entries);
    assert_eq!(interrupted_entries + cold_entries, 4588);
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let membership = store.lock_state().warm_memberships.get(&key).cloned().expect("published complete membership");
    assert!(membership.complete);
    assert_eq!(membership.members.len(), 4587);
    let (_, warm_entries) = catalog_finish_positive(&store, &request, &owner, 0);
    assert_eq!(warm_entries, 0);
    let changed_path = source.directory.join(format!("{}.jsonl", ids[2300]));
    let canonical = std::fs::canonicalize(&changed_path).unwrap();
    let old_stamp = membership.members.iter().find(|member| member.path == canonical).unwrap().stamp;
    std::fs::OpenOptions::new().append(true).open(&changed_path).unwrap()
        .write_all(format!("{}\n", tool_use_line("Grep")).as_bytes()).unwrap();
    let (_, changed_entries) = catalog_finish_positive(&store, &request, &owner, 0);
    assert_eq!(changed_entries, 0, "regular-file append must not re-enumerate an unchanged directory");
    let fresh = store.lock_state().warm_memberships.get(&key).cloned().expect("fresh complete membership");
    let new_stamp = fresh.members.iter().find(|member| member.path == canonical).unwrap().stamp;
    assert_eq!(fresh.members.len(), 4587);
    assert_eq!(old_stamp.identity, new_stamp.identity);
    assert!(new_stamp.size > old_stamp.size);
    assert_ne!(fresh.revision, membership.revision);
    store.assert_conserved();
}

#[test]
fn registered_catalog_later_saved_issue_does_not_repeat_whole_forest_holds() {
    let source = Source::new(&format!("{}\n", user("root")));
    let outside = Source::new("");
    let early = source.directory.join("early");
    let late = source.directory.join("late");
    std::fs::create_dir(&early).unwrap();
    std::fs::create_dir(&late).unwrap();
    std::os::unix::fs::symlink(&outside.path, late.join("unrelated.txt")).unwrap();
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":1,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
    let mut owner = context("catalog-saved-issue");
    owner.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["missing".to_owned()]);
    request.insert("roots", json!([late,early]));
    let (initial, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(initial["status"].as_str(), Some("permission_denied"), "{initial:?}");
    let depth = 64;
    let mut directory = early;
    for _ in 0..depth { directory.push("d"); std::fs::create_dir(&directory).unwrap(); }
    std::fs::write(directory.join("unrelated.txt"), "").unwrap();
    request.insert("deadline_unix_ms", json!(now_ms() + 750));
    let before = store.location_canonicalizations.load(Ordering::Relaxed);
    let (again, entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    let work = store.location_canonicalizations.load(Ordering::Relaxed) - before;
    assert_eq!(again["status"].as_str(), Some("permission_denied"), "{again:?}");
    assert_eq!(entries, depth + 1);
    assert!(work <= 32 * depth as usize + 1024, "saved later issue work={work} depth={depth}");
    store.assert_conserved();
}

#[test]
fn registered_catalog_retries_transient_directory_issues_in_dfs_order() {
    let source = Source::new("");
    let early = source.directory.join("early");
    let late = source.directory.join("late");
    for directory in [&early, &late] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("wanted.jsonl"), "").unwrap();
    }
    let store = store();
    let owner = context("catalog-retry-order");
    let mut reservation = store.reserve_projection(&owner, 4096).unwrap();
    let mut budget = CatalogBudget { store: &store, context: &owner, reservation: &mut reservation, attempt: 2, scratch_bytes: 0 };
    let roots = [json!(late),json!(early)];
    let mut catalog = SourceCatalog::new(&store, {
            let (key, reservation) = store.catalog_key(&roots, &owner).unwrap();
            let scope = key.as_bytes().try_into().unwrap();
            drop(key);
            drop(reservation);
            scope
        }, &roots, &owner, &mut budget).unwrap();
    catalog.pending.clear();
    for directory in [&early, &late] {
        let identity = SourceStamp::of(&std::fs::metadata(directory).unwrap()).identity;
        budget.reserve("injected transient I/O".len() + vec_growth(&catalog.retry_issues, 1)).unwrap();
        let node = catalog.directories.get_mut(&identity).unwrap();
        let before = node.owned_bytes();
        node.scan = CatalogScan::Issue { status: Status::Changed, reason: "injected transient I/O".to_owned(), position: 0, attempt: 1 };
        catalog.directory_bytes = catalog.directory_bytes - before + node.owned_bytes();
        catalog.retry_issues.push(identity);
        catalog.record_issue(identity, 0);
    }
    let mut usage = [0;20];
    let deadline = now_ms() + 750;
    catalog.advance(&store, &Cancellation::default(), deadline, 1, &mut usage, &mut budget).unwrap();
    let plan = catalog.read_plan(&Cancellation::default(), deadline, None, &mut budget).unwrap();
    let paths = catalog.try_locate(&store, &[json!("wanted")], &owner, &Cancellation::default(), deadline, &plan, &mut budget).unwrap().expect("earliest closed prefix");
    assert_eq!(paths.get("wanted"), Some(&early.join("wanted.jsonl")));
    assert_eq!(catalog.charge(), catalog.audit_charge());
    drop(catalog);
    drop(budget);
    drop(reservation);
    store.assert_conserved();
}

#[test]
fn registered_catalog_retry_admission_preserves_the_unpublished_frontier() {
    let source = Source::new("");
    for index in 0..5 {
        let directory = source.directory.join(format!("retry-{index}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("wanted.jsonl"), "").unwrap();
    }
    for background in [false, true] {
        let store = store();
        let mut owner = context("catalog-retry-admission");
        if background { owner.insert("work_class", json!("background")); }
        let mut reservation = store.reserve_projection(&owner, 4096).unwrap();
        let mut budget = CatalogBudget { store: &store, context: &owner, reservation: &mut reservation, attempt: 1, scratch_bytes: 0 };
        let roots = [json!(source.directory)];
        let mut catalog = SourceCatalog::new(&store, {
            let (key, reservation) = store.catalog_key(&roots, &owner).unwrap();
            let scope = key.as_bytes().try_into().unwrap();
            drop(key);
            drop(reservation);
            scope
        }, &roots, &owner, &mut budget).unwrap();
        let root_identity = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()).identity;
        let mut usage = [0;20];
        catalog.advance(&store, &Cancellation::default(), now_ms() + 750, usize::MAX, &mut usage, &mut budget).unwrap();
        catalog.sync_routes(&mut budget).unwrap();
        let identities: Vec<_> = catalog.directories.keys().filter(|identity| **identity != root_identity).copied().collect();
        let earliest = identities.iter().min_by(|left, right|
            catalog.route_index.get(*left).unwrap().order.cmp(&catalog.route_index.get(*right).unwrap().order)).unwrap();
        let expected = catalog.route_index.get(earliest).unwrap().path.join("wanted.jsonl");
        assert_eq!(identities.len(), 5);
        assert!(catalog.pending.capacity() < identities.len());
        for identity in &identities {
            budget.reserve("transient retry".len() + vec_growth(&catalog.retry_issues, 1)).unwrap();
            catalog.remove_directory_index(*identity);
            let node = catalog.directories.get_mut(identity).unwrap();
            let before = node.owned_bytes();
            node.edges.clear(); node.directories.clear(); node.conditional.clear(); node.issues.clear();
            node.payload_bytes = 0;
            node.scan = CatalogScan::Issue { status: Status::Changed, reason: "transient retry".to_owned(), position: 0, attempt: 1 };
            catalog.directory_bytes = catalog.directory_bytes - before + node.owned_bytes();
            catalog.retry_issues.push(*identity);
            catalog.record_issue(*identity, 0);
        }
        catalog.complete = false;
        budget.attempt = 2;
        let pending = catalog.pending.clone();
        let retries = catalog.retry_issues.clone();
        let issue_bound = catalog.issue_bound;
        let charge = catalog.charge();
        assert_eq!(charge, catalog.audit_charge());
        let growth = vec_growth(&catalog.pending, retries.len());
        assert!(growth > 0);
        let scratch_growth = LOCATE_USER_PATH_SLOTS.saturating_sub(budget.scratch_bytes);
        let headroom = scratch_growth + growth - 1;
        eprintln!("registered_retry_admission background={background} scratch_admitted={} scratch_growth={scratch_growth} retry_growth={growth} headroom={headroom}", budget.scratch_bytes);
        let cap = store.config.retained - if background { store.config.hook_bytes } else { 0 };
        let used = store.retained_accounted_bytes();
        let filler = store.reserve_projection(&owner, cap - used - headroom).unwrap();
        let error = catalog.advance(&store, &Cancellation::default(), now_ms() + 750, 1, &mut usage, &mut budget).unwrap_err();
        assert_eq!(error.status, Status::RetainedLimit);
        assert_eq!(catalog.pending, pending);
        assert_eq!(catalog.retry_issues, retries);
        assert_eq!(catalog.issue_bound, issue_bound);
        assert!(identities.iter().all(|identity|
            matches!(catalog.directories.get(identity).unwrap().scan, CatalogScan::Issue { attempt: 1, .. })));
        assert_eq!(catalog.charge(), charge);
        assert_eq!(catalog.charge(), catalog.audit_charge());
        drop(filler);
        let deadline = now_ms() + 750;
        catalog.advance(&store, &Cancellation::default(), deadline, 1, &mut usage, &mut budget).unwrap();
        let plan = catalog.read_plan(&Cancellation::default(), deadline, None, &mut budget).unwrap();
        let paths = catalog.try_locate(&store, &[json!("wanted")], &owner, &Cancellation::default(), deadline, &plan, &mut budget).unwrap().expect("earliest retry prefix");
        assert_eq!(paths.get("wanted"), Some(&expected));
        assert_eq!(catalog.charge(), catalog.audit_charge());
        drop(catalog);
        drop(budget);
        drop(reservation);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_id_validation_work_is_bounded_by_the_chunk() {
    let store = store();
    let mut measurements = Vec::new();
    for count in [128, 512, 1024] {
        let ids: Vec<_> = (0..count)
            .map(|index| json!(format!("session-{:04}", (index * 129 + 17) % count)))
            .collect();
        let before = store.catalog_id_comparisons.load(Ordering::Relaxed);
        store.validate_catalog_chunk(&ids).unwrap();
        let comparisons = store.catalog_id_comparisons.load(Ordering::Relaxed) - before;
        eprintln!("catalog_id_validation ids={count} comparisons={comparisons}");
        measurements.push((count, comparisons));
    }
    for (count, comparisons) in measurements {
        assert!(comparisons <= 32 * count, "ids={count} comparisons={comparisons}");
    }
}

#[test]
fn registered_catalog_id_validation_preserves_the_first_error() {
    let store = store();
    for (ids, reason) in [
        (json!(["same", "same", 7]), "invalid or duplicate session id"),
        (json!(["same", 7, "same"]), "invalid session id"),
        (json!([7, "same", "same"]), "invalid session id"),
        (json!(["same", "", 7]), "invalid or duplicate session id"),
        (json!(["a", "b", "a"]), "invalid or duplicate session id"),
    ] {
        let error = store.validate_catalog_chunk(ids.as_array().unwrap()).unwrap_err();
        assert_eq!(error.reason, reason);
    }
    store.validate_catalog_chunk(&[]).unwrap();
    store.validate_catalog_chunk(json!(["a", "b", "c"]).as_array().unwrap()).unwrap();
}

#[test]
fn registered_catalog_membership_rejects_a_directory_source() {
    let source = Source::new(&format!("{}\n", user("root")));
    let store = store();
    let owner = context("directory-source");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &[]);
    request.insert("direct_paths", json!(["/"]));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let result = store.build_warm_membership(&key, &request, &owner, &Cancellation::default(),
        &mut [0;20], &mut remaining);
    assert!(matches!(result, Err(error) if error.reason == "registered source must be a file"));
    store.assert_conserved();
}

#[test]
fn registered_catalog_membership_rejects_a_parent_retargeted_during_its_pin() {
    let source = Source::new(&format!("{}\n", user("root")));
    let left = source.directory.join("left");
    let right = source.directory.join("right");
    let alias = source.directory.join("alias");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    for directory in [&left, &right] {
        for id in ["first", "second"] {
            std::fs::write(directory.join(format!("{id}.jsonl")), format!("{}\n", user(id))).unwrap();
        }
    }
    std::os::unix::fs::symlink(&left, &alias).unwrap();
    let store = store();
    let owner = context("membership-parent-retarget");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["first".to_owned(), "second".to_owned()]);
    request.insert("roots", json!([alias]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let retargeted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *store.resolve_hook.lock().unwrap() = Some(Arc::new({
        let (alias, right, retargeted) = (alias.clone(), right.clone(), Arc::clone(&retargeted));
        move |path| {
            if path.parent() == Some(alias.as_path()) && !retargeted.swap(true, Ordering::Relaxed) {
                std::fs::remove_file(&alias).unwrap();
                std::os::unix::fs::symlink(&right, &alias).unwrap();
            }
        }
    }));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let result = store.build_warm_membership(&key, &request, &owner, &Cancellation::default(),
        &mut [0;20], &mut remaining);
    assert!(retargeted.load(Ordering::Relaxed));
    assert!(matches!(result, Err(error) if error.status == Status::Changed));
    store.assert_conserved();
}

fn catalog_cached_membership(store: &NativeStore, request: &Value, owner: &Value, revalidate: bool) -> WarmMembership {
    let key = NativeStore::warm_membership_key(request, owner).unwrap();
    let mut remaining = limits(request).unwrap();
    let (membership, mut reservation) = store.registered_membership(&key, request, owner,
        &Cancellation::default(), &mut [0; 20], &mut remaining, revalidate).unwrap();
    store.publish_warm_membership(&key, &membership, owner, reservation.as_mut()).unwrap();
    drop(reservation);
    membership
}

#[test]
fn registered_catalog_cached_membership_observes_root_retarget_with_old_target_alive() {
    for revalidate in [true, false] {
        let source = Source::new(&format!("{}\n", user("root")));
        let left = source.directory.join("left");
        let right = source.directory.join("right");
        let alias = source.directory.join("alias");
        std::fs::create_dir(&left).unwrap();
        std::fs::create_dir(&right).unwrap();
        for (directory, text) in [(&left, "left"), (&right, "right")] {
            std::fs::write(directory.join("wanted.jsonl"), format!("{}\n", user(text))).unwrap();
        }
        std::os::unix::fs::symlink(&left, &alias).unwrap();
        let store = store();
        let owner = context("catalog-cache-root-retarget");
        let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
        request.insert("roots", json!([alias]));
        request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let initial = catalog_cached_membership(&store, &request, &owner, true);
        assert_eq!(initial.members[0].path, std::fs::canonicalize(left.join("wanted.jsonl")).unwrap());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&right, &alias).unwrap();
        let fresh = catalog_cached_membership(&store, &request, &owner, revalidate);
        assert!(left.join("wanted.jsonl").is_file());
        assert_eq!(fresh.members[0].path, std::fs::canonicalize(right.join("wanted.jsonl")).unwrap());
        assert_ne!(initial.revision, fresh.revision);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_negative_lookup_observes_a_nested_source_creation() {
    let source = Source::new(&format!("{}\n", user("root")));
    let nested = source.directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let store = store();
    let owner = context("catalog-negative-created");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let first = store.registered_membership(&key, &request, &owner,
        &Cancellation::default(), &mut [0; 20], &mut remaining, true);
    assert!(matches!(first, Err(error) if error.status == Status::Incomplete));
    assert!(!store.lock_state().warm_memberships.contains_key(&key));
    let path = nested.join("wanted.jsonl");
    std::fs::write(&path, format!("{}\n", user("new"))).unwrap();
    let fresh = catalog_cached_membership(&store, &request, &owner, true);
    assert!(fresh.complete);
    assert_eq!(fresh.members[0].path, std::fs::canonicalize(path).unwrap());
    store.assert_conserved();
}

#[test]
fn registered_catalog_membership_certificate_survives_reads_and_same_file_appends() {
    let source = Source::new(&format!("{}\n", user("root")));
    let path = source.directory.join("wanted.jsonl");
    std::fs::write(&path, format!("{}\n", user("first"))).unwrap();
    let store = store();
    let owner = context("catalog-certificate-stability");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let initial = catalog_cached_membership(&store, &request, &owner, true);
    let unchanged = catalog_cached_membership(&store, &request, &owner, true);
    assert!(initial.catalog.is_some());
    assert_eq!(initial.catalog, unchanged.catalog);
    assert!(Arc::ptr_eq(&initial.members, &unchanged.members));
    std::fs::OpenOptions::new().append(true).open(&path).unwrap()
        .write_all(format!("{}\n", user("second")).as_bytes()).unwrap();
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let mut usage = [0; 20];
    let (fresh, mut reservation) = store.registered_membership(&key, &request, &owner,
        &Cancellation::default(), &mut usage, &mut remaining, true).unwrap();
    store.publish_warm_membership(&key, &fresh, &owner, reservation.as_mut()).unwrap();
    drop(reservation);
    assert_eq!(usage[17], 0);
    assert_eq!(initial.catalog, fresh.catalog);
    assert_ne!(initial.revision, fresh.revision);
    assert!(fresh.members[0].stamp.size > initial.members[0].stamp.size);
    store.assert_conserved();
}

#[test]
fn registered_catalog_membership_does_not_pin_eviction_or_accept_a_recreated_catalog() {
    let source = Source::new(&format!("{}\n", user("root")));
    std::fs::write(source.directory.join("wanted.jsonl"), format!("{}\n", user("wanted"))).unwrap();
    let store = store();
    let owner = context("catalog-certificate-incarnation");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let initial = catalog_cached_membership(&store, &request, &owner, true);
    let certificate = initial.catalog.unwrap();
    let weak = {
        let mut state = store.lock_state();
        let slot = state.catalogs.get(certificate.key()).unwrap();
        assert_eq!(Arc::strong_count(slot), 1);
        let weak = Arc::downgrade(slot);
        drop(state.remove_catalog(certificate.key()).unwrap());
        weak
    };
    assert!(weak.upgrade().is_none());
    let fresh = catalog_cached_membership(&store, &request, &owner, false);
    assert_eq!(initial.revision, fresh.revision);
    assert_ne!(certificate.incarnation, fresh.catalog.unwrap().incarnation);
    let mut remaining = limits(&request).unwrap();
    assert!(!store.catalog_membership_valid(Some(certificate), &request, &owner,
        &Cancellation::default(), &mut remaining).unwrap());
    store.assert_conserved();
}

#[test]
fn registered_catalog_membership_cannot_publish_after_its_catalog_is_removed() {
    let source = Source::new(&format!("{}\n", user("root")));
    std::fs::write(source.directory.join("wanted.jsonl"), format!("{}\n", user("wanted"))).unwrap();
    let store = store();
    let owner = context("catalog-certificate-publication");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let (membership, mut reservation) = store.build_warm_membership(&key, &request, &owner,
        &Cancellation::default(), &mut [0; 20], &mut remaining).unwrap();
    drop(store.lock_state().remove_catalog(membership.catalog.unwrap().key()).unwrap());
    let result = store.publish_warm_membership(&key, &membership, &owner, Some(&mut reservation));
    assert!(matches!(result, Err(error) if error.status == Status::Changed));
    assert!(!store.lock_state().warm_memberships.contains_key(&key));
    drop(reservation);
    store.assert_conserved();
}

#[test]
fn registered_catalog_membership_certificate_validation_honors_cancellation() {
    let source = Source::new(&format!("{}\n", user("root")));
    std::fs::write(source.directory.join("wanted.jsonl"), format!("{}\n", user("wanted"))).unwrap();
    let store = store();
    let owner = context("catalog-certificate-cancellation");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let membership = catalog_cached_membership(&store, &request, &owner, true);
    let cancel = Cancellation::default();
    cancel.cancel();
    let mut remaining = limits(&request).unwrap();
    let result = store.catalog_membership_valid(membership.catalog, &request, &owner, &cancel, &mut remaining);
    assert!(matches!(result, Err(error) if error.status == Status::Cancelled));
    let mut remaining = limits(&request).unwrap();
    assert!(store.catalog_membership_valid(membership.catalog, &request, &owner,
        &Cancellation::default(), &mut remaining).unwrap());
    store.assert_conserved();
}

#[test]
fn registered_catalog_certificate_allows_later_chunk_discovery_after_a_certified_prefix() {
    let source = Source::new(&format!("{}\n", user("root")));
    let later = source.directory.join("later");
    std::fs::create_dir(&later).unwrap();
    let first = source.directory.join("first.jsonl");
    let second = later.join("second.jsonl");
    for path in [&first, &second] {
        std::fs::write(path, format!("{}\n", user("member"))).unwrap();
    }
    let store = store();
    let owner = context("catalog-certificate-chunk-prefix");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["first".to_owned(), "second".to_owned()]);
    request.insert("roots", json!([later, first]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    request["limits"].insert("max_items", json!(1));
    let membership = catalog_cached_membership(&store, &request, &owner, true);
    assert!(membership.complete);
    assert_eq!(membership.members.len(), 2);
    assert_eq!(membership.members[0].path, std::fs::canonicalize(first).unwrap());
    assert_eq!(membership.members[1].path, std::fs::canonicalize(second).unwrap());
    store.assert_conserved();
}

#[test]
fn registered_catalog_certificate_rejects_an_earlier_root_retarget_during_later_discovery() {
    let source = Source::new(&format!("{}\n", user("root")));
    let later = source.directory.join("later");
    std::fs::create_dir(&later).unwrap();
    let left = source.directory.join("left.txt");
    let right = source.directory.join("right.txt");
    let alias = source.directory.join("first.jsonl");
    let second = later.join("second.jsonl");
    for path in [&left, &right, &second] {
        std::fs::write(path, format!("{}\n", user("member"))).unwrap();
    }
    std::os::unix::fs::symlink(&left, &alias).unwrap();
    let store = store();
    let owner = context("catalog-certificate-prefix-invalidation");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["first".to_owned(), "second".to_owned()]);
    request.insert("roots", json!([later, alias]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    request["limits"].insert("max_items", json!(1));
    let retargeted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new({
        let (alias, right, retargeted) = (alias.clone(), right.clone(), Arc::clone(&retargeted));
        move || {
            if !retargeted.swap(true, Ordering::Relaxed) {
                std::fs::remove_file(&alias).unwrap();
                std::os::unix::fs::symlink(&right, &alias).unwrap();
            }
        }
    })));
    let key = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let mut remaining = limits(&request).unwrap();
    let outcome = store.build_warm_membership(&key, &request, &owner,
        &Cancellation::default(), &mut [0; 20], &mut remaining);
    *store.location_metadata_hook.lock().unwrap() = None;
    assert!(retargeted.load(Ordering::Relaxed));
    assert!(matches!(outcome, Err(error) if error.status == Status::Changed));
    assert!(!store.lock_state().warm_memberships.contains_key(&key));
    store.assert_conserved();
}

fn catalog_ancestor_aba_preserves_fresh_membership(stream: bool) {
    let source = Source::new(&format!("{}\n", user("root")));
    let parent = source.directory.join("parent");
    let saved = source.directory.join("saved-parent");
    let replacement = source.directory.join("replacement-parent");
    let forest = parent.join("forest");
    std::fs::create_dir_all(&forest).unwrap();
    std::fs::create_dir_all(replacement.join("forest")).unwrap();
    let (ids, expected) = if stream {
        std::fs::create_dir(forest.join("child")).unwrap();
        std::fs::create_dir(replacement.join("forest/child")).unwrap();
        let wanted = forest.join("child/wanted.jsonl");
        std::fs::write(&wanted, format!("{}\n", user("wanted"))).unwrap();
        std::fs::write(replacement.join("forest/child/other.jsonl"), format!("{}\n", user("other"))).unwrap();
        (vec!["wanted".to_owned()], vec![wanted])
    } else {
        let mut expected = Vec::new();
        for id in ["first", "second"] {
            let path = forest.join(format!("{id}.jsonl"));
            std::fs::write(&path, format!("{}\n", user(id))).unwrap();
            expected.push(path);
        }
        (vec!["first".to_owned(), "second".to_owned()], expected)
    };
    let before = SourceStamp::of(&std::fs::metadata(&forest).unwrap());
    let child_before = stream.then(|| SourceStamp::of(&std::fs::metadata(forest.join("child")).unwrap()));
    let store = Arc::new(NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":16,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap());
    let owner = context(if stream { "catalog-opened-stream-aba" } else { "catalog-child-fact-aba" });
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &ids);
    request.insert("roots", json!([forest]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let swapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restored = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new({
        let store = Arc::downgrade(&store);
        let (parent, saved, replacement, swapped, restored) = (parent.clone(), saved.clone(), replacement.clone(), Arc::clone(&swapped), Arc::clone(&restored));
        move || {
            std::fs::rename(&parent, &saved).unwrap();
            std::fs::rename(&replacement, &parent).unwrap();
            swapped.store(true, Ordering::Relaxed);
            *store.upgrade().unwrap().location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new({
                let (parent, saved, replacement, restored) = (parent.clone(), saved.clone(), replacement.clone(), Arc::clone(&restored));
                move || {
                    std::fs::rename(&parent, &replacement).unwrap();
                    std::fs::rename(&saved, &parent).unwrap();
                    restored.store(true, Ordering::Relaxed);
                }
            })));
        }
    })));
    let (first, first_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    *store.location_metadata_hook.lock().unwrap() = None;
    assert!(swapped.load(Ordering::Relaxed));
    if !restored.load(Ordering::Relaxed) {
        std::fs::rename(&parent, &replacement).unwrap();
        std::fs::rename(&saved, &parent).unwrap();
    }
    assert_eq!(SourceStamp::of(&std::fs::metadata(&forest).unwrap()), before);
    if let Some(before) = child_before {
        assert_eq!(SourceStamp::of(&std::fs::metadata(forest.join("child")).unwrap()), before);
    }
    assert!(expected.iter().all(|path| path.is_file()));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let (fresh, fresh_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    eprintln!("registered_ancestor_aba stream={stream} first={:?} first_entries={first_entries} fresh={:?} fresh_entries={fresh_entries}", first["status"].as_str(), fresh["status"].as_str());
    store.assert_conserved();
    assert_eq!(fresh["status"].as_str(), Some("ok"), "{fresh:?}");
    let graph_id = fresh["data"]["handle"]["graph_id"].as_str().unwrap().to_owned();
    let state = store.lock_state();
    let graph = state.prepared_graphs[&graph_id].lock().unwrap();
    for path in expected {
        let canonical = std::fs::canonicalize(path).unwrap();
        assert!(graph.sources.iter().any(|source| source.path == canonical));
    }
}

#[test]
fn registered_catalog_ancestor_aba_does_not_share_another_directorys_stream() {
    catalog_ancestor_aba_preserves_fresh_membership(true);
}

#[test]
fn registered_catalog_ancestor_aba_does_not_cache_another_directorys_child_issue() {
    catalog_ancestor_aba_preserves_fresh_membership(false);
}

#[test]
fn registered_catalog_healthy_restricted_symlink_and_root_alias_are_reusable() {
    for (root_alias, unknown) in [(false, false), (true, false), (false, true), (true, true)] {
        let source = Source::new(&format!("{}\n", user("root")));
        let forest = source.directory.join("forest");
        let targets = source.directory.join("targets");
        let alias = source.directory.join("forest-alias");
        std::fs::create_dir(&forest).unwrap();
        std::fs::create_dir(&targets).unwrap();
        let target = targets.join("actual.jsonl");
        std::fs::write(&target, format!("{}\n", user("wanted"))).unwrap();
        std::os::unix::fs::symlink(&target, forest.join("wanted.jsonl")).unwrap();
        std::os::unix::fs::symlink(&forest, &alias).unwrap();
        let store = store();
        store.directory_force_unknown.store(unknown, Ordering::Relaxed);
        let mut owner = context("catalog-healthy-restricted-symlink");
        owner.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
        let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
        request.insert("roots", json!([if root_alias { alias } else { forest }]));
        request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let (cold, cold_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
        assert_eq!(cold["status"].as_str(), Some("ok"), "{cold:?}");
        assert_eq!(cold_entries, 1);
        request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let (warm, warm_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
        eprintln!("registered_healthy_symlink root_alias={root_alias} unknown={unknown} cold_entries={cold_entries} warm_entries={warm_entries}");
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm_entries, 0);
        let graph_id = warm["data"]["handle"]["graph_id"].as_str().unwrap().to_owned();
        {
            let state = store.lock_state();
            let graph = state.prepared_graphs[&graph_id].lock().unwrap();
            assert!(graph.sources.iter().any(|source| source.path == std::fs::canonicalize(&target).unwrap()));
        }
        request.insert("thread_ids", json!(["missing"]));
        for _ in 0..2 {
            request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
            let (missing, entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
            assert_eq!(missing["status"].as_str(), Some("incomplete"), "{missing:?}");
            assert_eq!(missing["reason"].as_str(), Some("registered membership is incomplete"), "{missing:?}");
            assert_eq!(entries, 0);
        }
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_cancelled_name_proof_retains_the_consumed_entry_without_reenumeration() {
    for unknown in [false, true] {
        let source = Source::new(&format!("{}\n", user("root")));
        let forest = source.directory.join("forest");
        std::fs::create_dir(&forest).unwrap();
        for id in ["first", "second"] {
            std::fs::write(forest.join(format!("{id}.jsonl")), format!("{}\n", user(id))).unwrap();
        }
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":1,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        store.directory_force_unknown.store(unknown, Ordering::Relaxed);
        let owner = context("catalog-cancelled-name-proof");
        let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let mut request = catalog_missing_request(&source, &root, &["first".to_owned(), "second".to_owned()]);
        request.insert("roots", json!([forest]));
        request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let cancel = Cancellation::default();
        let cancel_name = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() = Some(("catalog_name", Arc::new(move || cancel_name.cancel())));
        let (cancelled, first_entries) = catalog_counted_request(&store, &request, &owner, &cancel);
        *store.location_metadata_hook.lock().unwrap() = None;
        assert_eq!(cancelled["status"].as_str(), Some("cancelled"), "{cancelled:?}");
        assert_eq!(first_entries, 1);
        {
            let state = store.lock_state();
            let slot = state.catalogs.values().next().expect("native catalog owner");
            let cell = slot.cell.lock().unwrap();
            let CatalogCell::Idle(catalog) = &*cell else { panic!("cancelled catalog is settled"); };
            let pending = catalog.directories.values().find_map(|node| node.pending_name.as_ref()).expect("a consumed NAME must survive private cancellation");
            if unknown { assert_eq!(pending.entry.kind, libc::DT_UNKNOWN); }
            assert_eq!(catalog.charge(), catalog.audit_charge());
        }
        store.assert_conserved();
        request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
        let (fresh, remaining_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
        eprintln!("registered_cancelled_name unknown={unknown} first_entries={first_entries} remaining_entries={remaining_entries}");
        assert_eq!(fresh["status"].as_str(), Some("ok"), "{fresh:?}");
        assert_eq!(first_entries + remaining_entries, 2);
        store.assert_conserved();
    }
}

#[test]
fn registered_catalog_ancestor_aba_rejects_an_empty_foreign_stream() {
    let source = Source::new(&format!("{}\n", user("root")));
    let parent = source.directory.join("parent");
    let saved = source.directory.join("saved-parent");
    let replacement = source.directory.join("replacement-parent");
    let forest = parent.join("forest");
    std::fs::create_dir_all(forest.join("child")).unwrap();
    std::fs::create_dir_all(replacement.join("forest/child")).unwrap();
    let wanted = forest.join("child/wanted.jsonl");
    std::fs::write(&wanted, format!("{}\n", user("wanted"))).unwrap();
    let before = SourceStamp::of(&std::fs::metadata(&forest).unwrap());
    let store = Arc::new(NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":16,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap());
    let owner = context("catalog-empty-foreign-stream");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &["wanted".to_owned()]);
    request.insert("roots", json!([forest]));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let swapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    *store.location_metadata_hook.lock().unwrap() = Some(("entry", Arc::new({
        let (parent, saved, replacement, swapped) = (parent.clone(), saved.clone(), replacement.clone(), Arc::clone(&swapped));
        move || {
            std::fs::rename(&parent, &saved).unwrap();
            std::fs::rename(&replacement, &parent).unwrap();
            swapped.store(true, Ordering::Relaxed);
        }
    })));
    let (first, first_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    *store.location_metadata_hook.lock().unwrap() = None;
    assert!(swapped.load(Ordering::Relaxed), "first NAME observation was not reached: {first:?}");
    std::fs::rename(&parent, &replacement).unwrap();
    std::fs::rename(&saved, &parent).unwrap();
    assert_eq!(SourceStamp::of(&std::fs::metadata(&forest).unwrap()), before);
    assert_eq!(first["status"].as_str(), Some("changed"), "{first:?}");
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let (fresh, fresh_entries) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    eprintln!("registered_empty_foreign_stream first_entries={first_entries} fresh_entries={fresh_entries}");
    assert_eq!(fresh["status"].as_str(), Some("ok"), "{fresh:?}");
    assert_eq!(first_entries + fresh_entries, 2);
    store.assert_conserved();
}

#[test]
fn registered_catalog_pending_name_survives_earlier_frontier_reordering() {
    for unknown in [false, true] {
        for retry in [false, true] {
            let source = Source::new("");
            let early = source.directory.join("early");
            let late = source.directory.join("late");
            for directory in [&early, &late] {
                std::fs::create_dir(directory).unwrap();
                std::fs::write(directory.join("wanted.jsonl"), "").unwrap();
            }
            let store = store();
            store.directory_force_unknown.store(unknown, Ordering::Relaxed);
            let owner = context("catalog-pending-name-reorder");
            let mut reservation = store.reserve_projection(&owner, 4096).unwrap();
            let mut budget = CatalogBudget { store: &store, context: &owner, reservation: &mut reservation, attempt: 1, scratch_bytes: 0 };
            let roots = [json!(late), json!(early)];
            let scope = {
                let (key, reservation) = store.catalog_key(&roots, &owner).unwrap();
                let scope = key.as_bytes().try_into().unwrap();
                drop(key);
                drop(reservation);
                scope
            };
            let mut catalog = SourceCatalog::new(&store, scope, &roots, &owner, &mut budget).unwrap();
            let early_id = SourceStamp::of(&std::fs::metadata(&early).unwrap()).identity;
            let late_id = SourceStamp::of(&std::fs::metadata(&late).unwrap()).identity;
            let cancel = Cancellation::default();
            let cancel_name = cancel.clone();
            let names = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&names);
            *store.location_metadata_hook.lock().unwrap() = Some(("catalog_name", Arc::new(move || {
                if observed.fetch_add(1, Ordering::Relaxed) == 1 { cancel_name.cancel(); }
            })));
            let mut usage = [0;20];
            let error = catalog.advance(&store, &cancel, now_ms() + 30_000, usize::MAX, &mut usage, &mut budget).unwrap_err();
            *store.location_metadata_hook.lock().unwrap() = None;
            assert_eq!(error.status, Status::Cancelled);
            assert_eq!(usage[17], 2);
            assert_eq!(catalog.pending.last(), Some(&late_id));
            let CatalogScan::Open(reader) = &catalog.directories.get(&late_id).unwrap().scan else { panic!("late directory stays open"); };
            assert_eq!(Arc::strong_count(&reader.entries), 2);
            assert_eq!(catalog.charge(), catalog.audit_charge());
            if retry {
                catalog.remove_directory_index(early_id);
                budget.reserve("transient retry".len() + vec_growth(&catalog.retry_issues, 1)).unwrap();
                let node = catalog.directories.get_mut(&early_id).unwrap();
                let before = node.owned_bytes();
                node.edges.clear(); node.directories.clear(); node.conditional.clear(); node.issues.clear();
                node.payload_bytes = 0;
                node.scan = CatalogScan::Issue { status: Status::Changed, reason: "transient retry".to_owned(), position: 0, attempt: 1 };
                catalog.directory_bytes = catalog.directory_bytes - before + node.owned_bytes();
                catalog.retry_issues.push(early_id);
                catalog.record_issue(early_id, 0);
                budget.attempt = 2;
            } else {
                std::fs::File::open(&early).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
                catalog.refresh(&store, &Cancellation::default(), now_ms() + 30_000, &mut budget).unwrap();
                assert_eq!(catalog.pending.last(), Some(&early_id));
            }
            let cancel = Cancellation::default();
            let cancel_name = cancel.clone();
            *store.location_metadata_hook.lock().unwrap() = Some(("catalog_name", Arc::new(move || cancel_name.cancel())));
            let error = catalog.advance(&store, &cancel, now_ms() + 30_000, usize::MAX, &mut usage, &mut budget).unwrap_err();
            *store.location_metadata_hook.lock().unwrap() = None;
            assert_eq!(error.status, Status::Cancelled);
            assert_eq!(catalog.pending.last(), Some(&early_id));
            assert_eq!(usage[17], 3);
            for identity in [early_id, late_id] {
                let CatalogScan::Open(reader) = &catalog.directories.get(&identity).unwrap().scan else { panic!("both consumed NAME readers stay open"); };
                assert_eq!(Arc::strong_count(&reader.entries), 2);
            }
            assert_eq!(catalog.charge(), catalog.audit_charge());
            let charge = catalog.charge();
            let pending = catalog.pending.clone();
            let used = store.retained_accounted_bytes();
            let filler = store.reserve_projection(&owner, store.config.retained - used).unwrap();
            let error = catalog.advance(&store, &Cancellation::default(), now_ms() + 30_000, 0, &mut usage, &mut budget).unwrap_err();
            assert_eq!(error.status, Status::RetainedLimit);
            assert_eq!(catalog.pending, pending);
            assert_eq!(catalog.charge(), charge);
            assert_eq!(catalog.charge(), catalog.audit_charge());
            drop(filler);
            catalog.advance(&store, &Cancellation::default(), now_ms() + 30_000, 0, &mut usage, &mut budget).unwrap();
            let plan = catalog.read_plan(&Cancellation::default(), now_ms() + 30_000, None, &mut budget).unwrap();
            let paths = catalog.try_locate(&store, &[json!("wanted")], &owner, &Cancellation::default(), now_ms() + 30_000, &plan, &mut budget).unwrap().expect("earlier closed prefix wins before the parked late NAME");
            assert_eq!(paths.get("wanted"), Some(&early.join("wanted.jsonl")));
            let CatalogScan::Open(reader) = &catalog.directories.get(&late_id).unwrap().scan else { panic!("late NAME is still parked"); };
            assert_eq!(Arc::strong_count(&reader.entries), 2);
            catalog.advance(&store, &Cancellation::default(), now_ms() + 30_000, usize::MAX, &mut usage, &mut budget).unwrap();
            assert!(catalog.complete);
            assert_eq!(usage[17], 3);
            assert_eq!(catalog.charge(), catalog.audit_charge());
            eprintln!("catalog_pending_name_reorder unknown={unknown} retry={retry} entries={}", usage[17]);
            drop(catalog);
            drop(budget);
            drop(reservation);
            store.assert_conserved();
        }
    }
}

fn membership_control_fixture(count: usize) -> (Source, NativeStore, Value, Value) {
    let source = Source::new(&format!("{}\n", user("root")));
    let ids: Vec<_> = (0..count).map(|index| format!("member-{index:04}")).collect();
    for id in &ids {
        std::fs::write(source.directory.join(format!("{id}.jsonl")), format!("{}\n", user(id))).unwrap();
    }
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,
        "max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_leases":16,"reserved_hook_leases":1,
    })).unwrap();
    let owner = context("membership-progress");
    let root = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let mut request = catalog_missing_request(&source, &root, &ids);
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    request.insert("active_since_unix_ms", json!(1));
    request["limits"].insert("max_sources", json!(8192));
    (source, store, owner, request)
}

fn membership_control_force_deadline(
    store: &NativeStore,
    stage: &'static str,
    stop: usize,
) -> Arc<Mutex<Vec<usize>>> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&observed);
    let stopped = AtomicBool::new(false);
    *store.membership_hook.lock().unwrap() = Some(Arc::new(move |current, ordinal| {
        if current == stage {
            output.lock().unwrap().push(ordinal);
            if ordinal == stop && !stopped.swap(true, Ordering::Relaxed) {
                return Err(SnapshotError::new(Status::Deadline, "membership test deadline"));
            }
        }
        Ok(())
    }));
    observed
}

#[test]
fn cache_membership_materialization_resumes_after_a_deadline_with_a_moving_cutoff() {
    let (_source, store, owner, mut request) = membership_control_fixture(32);
    let observed = membership_control_force_deadline(&store, "source", 9);
    let first_scope = NativeStore::warm_membership_key(&request, &owner).unwrap();
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("deadline"), "{first:?}");
    assert_eq!(*observed.lock().unwrap(), (1..=9).collect::<Vec<_>>());
    store.assert_conserved();
    request.insert("id", json!("membership-next-window"));
    request.insert("active_since_unix_ms", json!(2));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    assert_ne!(first_scope, NativeStore::warm_membership_key(&request, &owner).unwrap());
    let (complete, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
    assert_eq!(*observed.lock().unwrap(), (1..=32).collect::<Vec<_>>(), "MEMBERSHIP_MATERIALIZATION_PREFIX_REPEATED");
    *store.membership_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn cache_membership_locate_resumes_completed_chunks_after_a_deadline() {
    let (_source, store, owner, mut request) = membership_control_fixture(2049);
    let observed = membership_control_force_deadline(&store, "locate", 1024);
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("deadline"), "{first:?}");
    assert_eq!(*observed.lock().unwrap(), [1024]);
    store.assert_conserved();
    request.insert("id", json!("membership-locate-next-window"));
    request.insert("active_since_unix_ms", json!(2));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let (complete, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
    assert_eq!(*observed.lock().unwrap(), [1024, 2048, 2049], "MEMBERSHIP_LOCATE_PREFIX_REPEATED");
    *store.membership_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn cache_membership_declared_rosters_keep_inactive_candidates_under_the_active_bound() {
    for count in [5677] {
        let (source, store, owner, mut request) = membership_control_fixture(count);
        let observed = membership_control_force_deadline(&store, "source", usize::MAX);
        for index in 0..count {
            let path = source.directory.join(format!("member-{index:04}.jsonl"));
            if index + 1 == count {
                std::fs::write(&path, format!("{}\n", tool_use_line("LastActive"))).unwrap();
            } else if index == 0 {
                std::fs::write(&path, format!("{}\n", tool_use_line("InactiveHidden"))).unwrap();
            }
            let file = std::fs::OpenOptions::new().write(true)
                .open(source.directory.join(format!("member-{index:04}.jsonl"))).unwrap();
            let seconds = if index + 1 == count { 2000 } else { 1000 };
            file.set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds))).unwrap();
        }
        request.insert("active_since_unix_ms", json!(1_500_000));
        request["limits"].insert("max_sources", json!(1));
        assert_eq!(request["roots"].as_array().unwrap().len() + request["thread_ids"].as_array().unwrap().len(), count + 1);
        let mut completed = None;
        for _ in 0..20 {
            request.insert("deadline_unix_ms", json!(now_ms() + 750));
            let (result, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
            if result["status"].as_str() == Some("deadline") {
                store.assert_conserved();
                continue;
            }
            assert_eq!(result["status"].as_str(), Some("ok"), "declared={} {result:?}", count + 1);
            completed = Some(result);
            break;
        }
        let completed = completed.expect("membership did not complete within current caller deadlines");
        assert_eq!(*observed.lock().unwrap(), (1..=count).collect::<Vec<_>>(), "declared roster collection repeated or omitted a member");
        *store.membership_hook.lock().unwrap() = None;
        assert!(graph_has_tool(&store, &source, &owner, &completed, "LastActive"));
        assert!(!graph_has_tool(&store, &source, &owner, &completed, "InactiveHidden"));
        assert!(!graph_has_tool(&store, &source, &owner, &completed, "Missing"));
        store.assert_conserved();
    }
}

#[test]
fn cache_membership_bound_refusal_does_not_discard_collection_or_poison_a_new_caller() {
    let (source, store, owner, mut request) = membership_control_fixture(32);
    for index in 0..32 {
        let file = std::fs::OpenOptions::new().write(true)
            .open(source.directory.join(format!("member-{index:04}.jsonl"))).unwrap();
        let seconds = if index == 31 { 2000 } else { 1000 };
        file.set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds))).unwrap();
    }
    let observed = membership_control_force_deadline(&store, "source", usize::MAX);
    request.insert("active_since_unix_ms", json!(500_000));
    request["limits"].insert("max_sources", json!(1));
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert_eq!(first["reason"].as_str(), Some("registered source bound exhausted"));
    assert!(first["cursor"].is_null());
    assert!(first["data"].is_null());
    assert_eq!(*observed.lock().unwrap(), (1..=32).collect::<Vec<_>>());
    store.assert_conserved();
    request.insert("id", json!("membership-narrower-current-window"));
    request.insert("active_since_unix_ms", json!(1_500_000));
    let (complete, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
    assert_eq!(*observed.lock().unwrap(), (1..=32).collect::<Vec<_>>(), "source-bound refusal discarded collection");
    request.insert("active_since_unix_ms", json!(500_000));
    let (wider, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(wider["status"].as_str(), Some("incomplete"), "{wider:?}");
    assert_eq!(wider["reason"].as_str(), Some("registered source bound exhausted"));
    assert!(wider["cursor"].is_null());
    assert!(wider["data"].is_null());
    assert_eq!(*observed.lock().unwrap(), (1..=32).collect::<Vec<_>>(), "new caller reused collection as a source-bound waiver");
    *store.membership_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn cache_membership_append_during_a_deadline_gap_never_publishes_old_selected_stamps() {
    let (source, store, owner, mut request) = membership_control_fixture(32);
    let observed = membership_control_force_deadline(&store, "source", 9);
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("deadline"));
    let parent_before = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
    let selected = source.directory.join("member-0000.jsonl");
    let selected_before = SourceStamp::of(&std::fs::metadata(&selected).unwrap());
    std::fs::OpenOptions::new().append(true).open(&selected)
        .unwrap().write_all(format!("{}\n", tool_use_line("Grep")).as_bytes()).unwrap();
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.directory).unwrap()), parent_before);
    let selected_after = SourceStamp::of(&std::fs::metadata(&selected).unwrap());
    assert_eq!(selected_before.identity, selected_after.identity);
    assert!(selected_after.size > selected_before.size);
    request.insert("active_since_unix_ms", json!(2));
    let (next, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(*observed.lock().unwrap(), (1..=32).collect::<Vec<_>>());
    *store.membership_hook.lock().unwrap() = None;
    let complete = if next["status"].as_str() == Some("changed") {
        assert!(next["data"].is_null());
        let (fresh, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
        fresh
    } else { next };
    assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
    assert!(graph_has_tool(&store, &source, &owner, &complete, "Grep"));
    store.assert_conserved();
}

#[test]
fn cache_membership_partial_sidechain_reader_closes_on_expiry_after_cancellation() {
    let (source, store, owner, request) = membership_control_fixture(32);
    let directory = sidechain_directory(&source.directory, OsStr::new("member-0000"));
    std::fs::create_dir_all(&directory).unwrap();
    for index in 0..32 {
        std::fs::write(directory.join(format!("child-{index:04}.jsonl")), format!("{}\n", user("child"))).unwrap();
    }
    let observed = membership_control_force_deadline(&store, "sidechain_entry", 7);
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("deadline"), "{first:?}");
    assert_eq!(*observed.lock().unwrap(), (1..=7).collect::<Vec<_>>());
    let key = NativeStore::membership_collection_key(&request, &owner).unwrap();
    let reader = {
        let state = store.lock_state();
        let slot = state.warm_memberships.get(&key).unwrap();
        let cell = slot.cell.lock().unwrap();
        let MembershipCell::Pending(build) = &*cell else { panic!("partial membership") };
        let Some(MembershipListing::Reading { entries, children, .. }) = &build.listing else { panic!("partial reader") };
        assert_eq!(children.len(), 7);
        Arc::downgrade(&entries.entries)
    };
    store.assert_conserved();
    let cancel = Cancellation::default();
    cancel.cancel();
    let cancelled = store.request(&request, &owner, &cancel);
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"));
    assert!(reader.upgrade().is_some());
    {
        let mut state = store.lock_state();
        state.warm_memberships.get(&key).unwrap().expires.store(0, Ordering::Release);
        NativeStore::prune(&mut state);
        assert!(!state.warm_memberships.contains_key(&key));
    }
    assert!(reader.upgrade().is_none());
    *store.membership_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn cache_membership_one_producer_preserves_other_callers_deadlines_and_store_progress() {
    let (_source, store, owner, request) = membership_control_fixture(8);
    let store = Arc::new(store);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_drop = CatalogOwnerRelease(Arc::clone(&gate));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let waits = Arc::new(AtomicUsize::new(0));
    let source_steps = Arc::new(Mutex::new(Vec::new()));
    *store.membership_hook.lock().unwrap() = Some(Arc::new({
        let (gate, waits, source_steps) = (Arc::clone(&gate), Arc::clone(&waits), Arc::clone(&source_steps));
        move |stage, ordinal| {
            if stage == "wait" {
                waits.fetch_add(1, Ordering::Relaxed);
            }
            if stage == "source" {
                source_steps.lock().unwrap().push(ordinal);
                if ordinal == 1 {
                    entered_tx.send(()).unwrap();
                    let (lock, ready) = &*gate;
                    let released = lock.lock().unwrap();
                    let (released, _) = ready.wait_timeout_while(released, Duration::from_secs(5), |released| !*released).unwrap();
                    if !*released {
                        return Err(SnapshotError::new(Status::Deadline, "membership gate expired"));
                    }
                }
            }
            Ok(())
        }
    }));
    let producer = {
        let (store, owner, request) = (Arc::clone(&store), owner.clone(), request.clone());
        std::thread::spawn(move || catalog_counted_request(&store, &request, &owner, &Cancellation::default()).0)
    };
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    let observations = entered.is_ok().then(|| {
        let mut current = request.clone();
        current.insert("deadline_unix_ms", json!(now_ms() + 50));
        current.insert("active_since_unix_ms", json!(2));
        let current = store.request(&current, &owner, &Cancellation::default());
        let stats = store.request(&json!({"schema":SCHEMA,"id":"membership-stats","operation":"stats"}), &owner, &Cancellation::default());
        (current, stats)
    });
    drop(release_on_drop);
    let produced = producer.join();
    assert!(entered.is_ok(), "membership producer did not reach the owned gate");
    let produced = produced.unwrap();
    assert_eq!(produced["status"].as_str(), Some("ok"), "{produced:?}");
    let (current, stats) = observations.unwrap();
    assert_eq!(current["status"].as_str(), Some("deadline"), "{current:?}");
    assert_eq!(stats["status"].as_str(), Some("ok"), "{stats:?}");
    assert!(waits.load(Ordering::Relaxed) > 0);
    assert_eq!(*source_steps.lock().unwrap(), (1..=8).collect::<Vec<_>>());
    *store.membership_hook.lock().unwrap() = None;
    store.assert_conserved();
}

#[test]
fn cache_membership_shared_collection_never_shares_public_handles_between_claimants() {
    let (source, store, owner, request) = membership_control_fixture(4);
    let (first, _) = catalog_counted_request(&store, &request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
    let mut other = owner.clone();
    other.insert("claimant", json!("another-membership-caller"));
    let root = finish(&store, store.request(&acquire(&source.path), &other, &Cancellation::default()), &other);
    let mut current = request.clone();
    current["view"].insert("handle", handle(&root).clone());
    let (second, _) = catalog_counted_request(&store, &current, &other, &Cancellation::default());
    assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
    assert_ne!(first["data"]["handle"]["graph_id"], second["data"]["handle"]["graph_id"]);
    let template = acquire(&source.path);
    let foreign = store.request(&json!({
        "schema":SCHEMA,"id":"foreign-membership-handle","operation":"query_graph",
        "handle":first["data"]["handle"],"selectors":[],
        "query":{"kind":"has_tool","pattern":"Missing","subagents":true},
        "deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"],
    }), &other, &Cancellation::default());
    assert_eq!(foreign["status"].as_str(), Some("stale_handle"), "{foreign:?}");
    assert!(foreign["data"].is_null());
    assert!(!graph_has_tool(&store, &source, &other, &second, "Missing"));
    store.assert_conserved();
}

#[test]
fn cache_membership_warm_pages_reject_a_changed_cutoff_before_skipping_earlier_members() {
    let (source, store, owner, mut request) = registered_warming(3);
    for index in 0..3 {
        let file = std::fs::OpenOptions::new().write(true)
            .open(source.directory.join(format!("thread-{index:04}.jsonl"))).unwrap();
        let seconds = if index == 0 { 1000 } else { 2000 };
        file.set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds))).unwrap();
    }
    request.insert("active_since_unix_ms", json!(1_500_000));
    request.insert("deadline_unix_ms", json!(now_ms() + 30_000));
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
    assert_eq!(first["data"]["complete"].as_bool(), Some(true));
    request.insert("start_index", first["data"]["next_index"].clone());
    request.insert("membership_revision", first["data"]["membership_revision"].clone());
    request.insert("active_since_unix_ms", json!(500_000));
    let changed = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(changed["status"].as_str(), Some("changed"), "{changed:?}");
    assert!(changed["data"].is_null());
    assert_eq!(changed["usage"]["source_bytes_read"].as_u64(), Some(0));
    request.insert("start_index", json!(0));
    request.insert("membership_revision", Value::new_null());
    let fresh = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(fresh["status"].as_str(), Some("ok"), "{fresh:?}");
    assert_eq!(fresh["data"]["complete"].as_bool(), Some(true));
    assert!(fresh["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    assert_ne!(first["data"]["membership_revision"], fresh["data"]["membership_revision"]);
    store.assert_conserved();
}

#[test]
fn cache_membership_real_child_growth_refusal_retains_the_pending_entry() {
    let source = Source::new(&format!("{}\n", user("root")));
    let directory = sidechain_directory(
        source.path.parent().unwrap(),
        source.path.file_stem().unwrap(),
    );
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("only-child.jsonl"), format!("{}\n", tool_use_line("Read"))).unwrap();
    let config = json!({
        "max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,
        "max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_leases":16,"reserved_hook_leases":1,
    });
    let store = Arc::new(NativeStore::new(&config).unwrap());
    let mut owner = context("membership-real-child-admission");
    owner.insert("work_class", json!("background"));
    let template = acquire(&source.path);
    let request = json!({
        "schema":SCHEMA,"id":"membership-real-child-admission","operation":"warm_registered",
        "classifier":{"id":"native","version":"1"},"thread_ids":[],"roots":[],
        "direct_paths":[source.path.to_string_lossy().as_ref()],"start_index":0,
        "membership_revision":null,"deadline_unix_ms":now_ms()+30_000,"limits":template["limits"],
    });
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_drop = CatalogOwnerRelease(Arc::clone(&gate));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let admissions = Arc::new(Mutex::new(Vec::new()));
    let sources = Arc::new(Mutex::new(Vec::new()));
    *store.membership_hook.lock().unwrap() = Some(Arc::new({
        let gate = Arc::clone(&gate);
        let admissions = Arc::clone(&admissions);
        let sources = Arc::clone(&sources);
        let paused = AtomicBool::new(false);
        move |stage, ordinal| {
            if stage == "source" {
                sources.lock().unwrap().push(ordinal);
            }
            if stage == "sidechain_admission" {
                admissions.lock().unwrap().push(ordinal);
                if !paused.swap(true, Ordering::Relaxed) {
                    entered_tx.send(()).unwrap();
                    let released = gate.0.lock().unwrap();
                    let (released, _) = gate.1.wait_timeout_while(
                        released, Duration::from_secs(5), |released| !*released,
                    ).unwrap();
                    if !*released {
                        return Err(SnapshotError::new(Status::Deadline, "admission fixture gate expired"));
                    }
                }
            }
            Ok(())
        }
    }));
    let producer = {
        let (store, request, owner) = (Arc::clone(&store), request.clone(), owner.clone());
        std::thread::spawn(move || store.request(&request, &owner, &Cancellation::default()))
    };
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    let filler = entered.as_ref().ok().map(|_| {
        let cap = store.memory_cap(&owner).unwrap();
        let used = store.retained_accounted_bytes();
        store.reserve_projection(&owner, cap.checked_sub(used).unwrap())
    });
    drop(release_on_drop);
    let first = producer.join();
    assert!(entered.is_ok(), "actual sidechain vector admission was not reached");
    let filler = filler.unwrap().expect("unrelated live reservation must fit before child vector growth");
    let first = first.unwrap();
    assert_eq!(first["status"].as_str(), Some("retained_limit"), "{first:?}");
    assert_eq!(first["reason"].as_str(), Some("accounted storage admission exhausted"));
    assert!(first["data"].is_null());
    assert!(first["cursor"].is_null());
    assert_eq!(first["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert_eq!(*admissions.lock().unwrap(), [1]);
    assert_eq!(*sources.lock().unwrap(), [1]);
    store.assert_conserved();
    drop(filler);
    let mut blocked = request.clone();
    blocked["limits"].insert("max_discovery_entries", json!(0));
    let blocked = store.request(&blocked, &owner, &Cancellation::default());
    assert_eq!(blocked["status"].as_str(), Some("incomplete"), "{blocked:?}");
    assert_eq!(blocked["reason"].as_str(), Some("registered sidechain discovery budget exhausted"));
    assert_eq!(blocked["usage"]["discovery_entries_examined"].as_u64(), Some(0));
    assert_eq!(*admissions.lock().unwrap(), [1]);
    assert_eq!(*sources.lock().unwrap(), [1]);
    store.assert_conserved();
    let mut resumed_request = request.clone();
    resumed_request["limits"].insert("max_discovery_entries", json!(1));
    let resumed = store.request(&resumed_request, &owner, &Cancellation::default());
    assert_eq!(resumed["status"].as_str(), Some("ok"), "{resumed:?}");
    assert_eq!(resumed["data"]["complete"].as_bool(), Some(true));
    assert_eq!(resumed["data"]["next_index"].as_u64(), Some(2));
    assert_eq!(resumed["usage"]["discovery_entries_examined"].as_u64(), Some(1));
    assert_eq!(*admissions.lock().unwrap(), [1, 1]);
    assert_eq!(*sources.lock().unwrap(), [1], "real growth refusal restarted selected-source collection");
    *store.membership_hook.lock().unwrap() = None;
    let oracle = NativeStore::new(&config).unwrap();
    let expected = oracle.request(&resumed_request, &owner, &Cancellation::default());
    assert_eq!(expected["status"].as_str(), Some("ok"), "{expected:?}");
    assert_eq!(expected["data"]["complete"].as_bool(), Some(true));
    assert_eq!(resumed["data"]["next_index"], expected["data"]["next_index"]);
    assert_eq!(resumed["data"]["membership_revision"], expected["data"]["membership_revision"], "R1_PENDING_ENTRY_LOST_AFTER_REAL_GROWTH_REFUSAL");
    store.assert_conserved();
    oracle.assert_conserved();
}

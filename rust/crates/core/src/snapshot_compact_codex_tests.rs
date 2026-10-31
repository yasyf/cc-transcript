fn compact_output(id: &str, exit_code: i64) -> String {
    format!("{}\n", json!({"timestamp":"2026-01-02T03:04:08Z","type":"response_item","payload":{
        "type":"function_call_output","call_id":id,"output":{"metadata":{"exit_code":exit_code},"output":"result"}}}))
}

fn compact_error_query(store: &NativeStore, graph: &Value, owner: &Value) -> (Value, [u64; 20]) {
    let mut request = hydration_query_request(graph, "unused");
    request.insert("query", json!({"kind":"has_error","subagents":true}));
    hydration_run_query(store, &request, owner)
}

fn compact_full_oracle(path: &Path) -> crate::snapshot_prepared::PreparedFacts {
    let bytes = std::fs::read(path).unwrap();
    let parsed = parse_codex_bytes(&bytes);
    let entries = lower(&parsed).entries;
    let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
    let snapshot = TranscriptSnapshot::from_complete_entries(String::new(), path.to_path_buf(), stamp,
        Provider::Codex, "oracle".to_owned(), entries);
    let bound = WorkLimits { max_read_bytes: 1024*1024, max_source_read_bytes:1024*1024,
        max_events:4096, max_items:256, max_output_bytes:1024*1024,
        max_discovery_entries:4096, max_sources:128, deadline_unix_ms:now_ms()+30_000 };
    crate::snapshot_projection::prepare_facts(&snapshot, &json!([]), &bound,
        &Cancellation::default(), FactsNeed::Index).unwrap().0
}

#[test]
fn prepared_cache_compact_codex_late_duplicate_results_match_complete_projection() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-results");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let initial = compact_error_query(&store, &graph, &owner).0;
    assert_eq!(initial["status"].as_str(), Some("ok"));
    assert_eq!(initial["data"]["value"].as_bool(), Some(false));
    let initial_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let old = {
        let mut state = store.lock_state();
        let cached = state.cached_prepared_facts(&initial_stamp.identity);
        assert!(cached.append.is_some());
        assert!(!state.latest.contains_key(&initial_stamp.identity));
        let facts = Arc::clone(&cached.facts);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_inputs = sonic_rs::to_string(&old.facts().inputs).unwrap();
    let suffixes = [
        (compact_output("initial-0", 1), true),
        (compact_output("initial-0", 0), false),
        (compact_output("not-yet-called", 1), false),
        (hydration_tool("late-call", "not-yet-called"), true),
        (hydration_tool("duplicate-call", "initial-0"), true),
        (compact_output("not-yet-called", 0), false),
    ];
    for (suffix, expected) in suffixes {
        std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
        let fresh = hydration_graph(&store, &root, &paths, &owner);
        hydration_observations(&store);
        let (answer, usage) = compact_error_query(&store, &fresh, &owner);
        assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
        assert_eq!(answer["data"]["value"].as_bool(), Some(expected));
        let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
        let actual = Arc::clone(&store.lock_state().cached_prepared_facts(&stamp.identity).facts);
        let complete = compact_full_oracle(&paths[0]);
        for column in ["calls", "commands", "edited_files", "skills"] {
            assert_eq!(sonic_rs::to_string(&actual.inputs[column]).unwrap(),
                sonic_rs::to_string(&complete.inputs[column]).unwrap(), "column {column}");
        }
        assert_eq!(sonic_rs::to_string(&actual.inputs).unwrap(), sonic_rs::to_string(&complete.inputs).unwrap());
        assert_eq!(actual.has_error, complete.has_error);
        assert_eq!(actual.has_error, expected);
        assert!(!actual.is_full());
        eprintln!("compact_result expected_error={expected} suffix={} read={} full_lowerings={}",suffix.len(),usage[1],usage[15]);
        assert!(usage[1] <= suffix.len() as u64 + 256);
        assert_eq!(usage[15], 0);
        assert!(!old.facts().has_error);
        assert_eq!(sonic_rs::to_string(&old.facts().inputs).unwrap(), old_inputs);
        assert_eq!(store.lock_state().cached_prepared_facts(&stamp.identity).stamp, stamp);
        store.assert_conserved();
    }
    drop(old);
    store.assert_conserved();
}

#[test]
fn prepared_cache_compact_codex_owner_retirement_keeps_only_guarded_metadata() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-owner");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let weak = Arc::downgrade(store.lock_state().cached_prepared_facts(&old_stamp.identity).append.as_ref().unwrap());
    let suffix = hydration_tool("next", "next");
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let borrow = store.previous_compact_codex(stamp, &owner, &json!({"id":"native","version":"1"})).unwrap().unwrap();
    assert_eq!(store.lock_state().remove_identity_facts(stamp.identity), 1);
    assert!(weak.upgrade().is_some());
    assert_eq!(borrow.append.stamp, old_stamp);
    assert!(store.lock_state().identity_facts(stamp.identity).next().is_none());
    store.assert_conserved();
    drop(borrow);
    assert!(weak.upgrade().is_none());
    store.assert_conserved();
}

#[test]
fn prepared_cache_compact_codex_refusal_cancel_and_full_need_keep_authority() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-refusal");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let classifier = json!({"id":"native","version":"1"});
    let old_key = PreparedDiskKey::new(stamp, owner["registry_generation"].as_str().unwrap(),
        "hook", &owner["authority"], &classifier).unwrap();
    let suffix = hydration_tool("new-refusal", "new-refusal");
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let token = Cancellation::default(); token.cancel();
    assert_eq!(store.request(&hydration_query_request(&fresh,"absent"), &owner,&token)["status"].as_str(), Some("cancelled"));
    let mut refused = hydration_query_request(&fresh,"absent");
    refused["limits"].insert("max_source_read_bytes",json!(1));
    let refused = finish_prepared(&store,store.request(&refused,&owner,&Cancellation::default()),&owner);
    assert_eq!(refused["status"].as_str(),Some("incomplete"),"{refused:?}");
    assert_ne!(refused["data"]["value"].as_bool(),Some(false));
    assert!(store.lock_state().prepared_facts.contains_key(&old_key));
    let (success, _) = hydration_tool_query(&store,&fresh,"new-refusal",&owner);
    assert_eq!(success["data"]["value"].as_bool(),Some(true));
    let mut full = hydration_query_request(&fresh,"unused");
    full.insert("query",json!({"kind":"has_override","token":"x","invalidated_by":[],"subagents":true}));
    let (full,usage) = hydration_run_query(&store,&full,&owner);
    assert_eq!(full["status"].as_str(),Some("ok"),"{full:?}");
    assert!(usage[15]>0,"Full must hydrate actual text");
    store.assert_conserved();
}

#[test]
fn prepared_cache_compact_codex_cancel_and_exact_refusal_never_publish_a_successor() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-inflight-refusal");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let classifier = json!({"id":"native","version":"1"});
    let suffix = hydration_tool("never-published", "never-published");
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let new_key = PreparedDiskKey::new(stamp,owner["registry_generation"].as_str().unwrap(),
        "hook",&owner["authority"],&classifier).unwrap();
    let mut request = acquire(&paths[0]);
    request.insert("limits", hydration_limits());
    let mut work = limits(&request).unwrap();
    let cancellation = Cancellation::default();
    let trigger = cancellation.clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || trigger.cancel()));
    let mut usage = [0;20];
    let failed = store.append_compact_codex(&paths[0],stamp,&owner,&classifier,
        &mut work,&cancellation,&mut usage);
    match failed {
        Err(error) => assert_eq!(error.status,Status::Cancelled),
        Ok(_) => panic!("cancelled compact append published facts"),
    }
    assert!(usage[1]>0);
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(store.lock_state().cached_prepared_facts(&stamp.identity).stamp,old_stamp);
    store.assert_conserved();
    let used = NativeStore::retained_total(&mut store.lock_state());
    let hold = store.reserve_projection(&owner,store.memory_cap(&owner).unwrap()-used).unwrap();
    let mut work = limits(&request).unwrap();
    let mut usage = [0;20];
    let failed = store.append_compact_codex(&paths[0],stamp,&owner,&classifier,
        &mut work,&Cancellation::default(),&mut usage);
    match failed {
        Err(error) => assert_eq!(error.status,Status::RetainedLimit),
        Ok(_) => panic!("refused compact append published facts"),
    }
    assert_eq!(usage[1],0);
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(store.lock_state().cached_prepared_facts(&stamp.identity).stamp,old_stamp);
    drop(hold);
    store.assert_conserved();
}

fn compact_assistant_line(text: &str, echo: bool) -> String {
    let line = if echo {
        json!({"timestamp":"2026-01-02T03:04:09Z","type":"response_item","payload":{
            "type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
    } else {
        json!({"timestamp":"2026-01-02T03:04:09Z","type":"event_msg","payload":{
            "type":"agent_message","message":text}})
    };
    format!("{line}\n")
}

#[test]
fn prepared_cache_compact_codex_capacity_boundary_admits_successor_and_temporary_rehash() {
    for echo in [false, true] {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths = hydration_sources(&source, 1);
        let target = if echo {
            HashSet::<String>::with_capacity(1024).capacity()
        } else {
            HashSet::<[u8;32]>::with_capacity(1024).capacity()
        };
        let start = usize::from(!echo);
        let mut raw = hydration_codex(0);
        for at in start..target {
            raw.push_str(&compact_assistant_line(&format!("capacity-{at}"),echo));
        }
        assert!(raw.len()<1024*1024);
        std::fs::write(&paths[0],raw).unwrap();
        let store = hydration_store();
        store.recent_codex_raw_limit.store(0,Ordering::Relaxed);
        let owner = context("compact-capacity-boundary");
        let root = hydration_root(&store,&source,&owner);
        let graph = hydration_graph(&store,&root,&paths,&owner);
        hydration_false(&hydration_query(&store,&graph,"definitely-absent",&owner).0);
        let suffix = compact_assistant_line("capacity-new-boundary",echo);
        std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
        let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
        let previous = store.previous_compact_codex(stamp,&owner,&json!({"id":"native","version":"1"})).unwrap().unwrap();
        let old = &previous.append.index;
        let (len,capacity) = if echo {
            (old.assistant_echoes.len(),old.assistant_echoes.capacity())
        } else {
            (old.assistant_events.len(),old.assistant_events.capacity())
        };
        assert_eq!((len,capacity),(target,target));
        let old_bytes = old.accounted_bytes();
        let mut parsed = parse_codex_bytes(suffix.as_bytes());
        for entry in &mut parsed.entries { entry.line_index += old.lines; }
        assert!(old.fallback_reason(&parsed).is_none());
        let admitted = old.compact_append_bound(&parsed);
        assert!(admitted.successor>old_bytes);
        assert!(admitted.append_peak>admitted.successor);
        assert!(admitted.lower_peak>0);
        let fresh = hydration_graph(&store,&root,&paths,&owner);
        let (answer,usage) = hydration_query(&store,&fresh,"definitely-absent",&owner);
        hydration_false(&answer);
        let state = store.lock_state();
        let new = &state.cached_prepared_facts(&stamp.identity).append.as_ref().unwrap().index;
        let new_capacity = if echo { new.assistant_echoes.capacity() } else { new.assistant_events.capacity() };
        assert!(new_capacity>capacity);
        assert!(new.accounted_bytes()<=admitted.successor);
        assert_eq!(old.accounted_bytes(),old_bytes);
        eprintln!("compact_capacity echo={echo} old_capacity={capacity} new_capacity={new_capacity} old_bytes={old_bytes} successor_bound={} actual={} temporary={} suffix={} source_bytes={}",admitted.successor,new.accounted_bytes(),admitted.lower_peak,suffix.len(),usage[1]);
        drop(state);
        assert!(usage[1]<=suffix.len() as u64+256);
        assert_eq!(usage[15],0);
        store.assert_conserved();
        drop(previous);
        store.assert_conserved();
    }
}

fn compact_read_countdown(
    store: Weak<NativeStore>,
    remaining: usize,
    action: Arc<dyn Fn() + Send + Sync>,
) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        if remaining==1 {
            action();
        } else {
            let owner = store.upgrade().unwrap();
            *owner.read_hook.lock().unwrap() = Some(compact_read_countdown(
                store.clone(),remaining-1,Arc::clone(&action)));
        }
    })
}

#[test]
fn prepared_cache_compact_codex_final_seal_parent_swap_rejects_positive_and_keeps_old_facts() {
    let source = Source::new(&format!("{}\n",user("root")));
    let parent = source.directory.join("original-parent");
    let parked = source.directory.join("parked-parent");
    let replacement = source.directory.join("replacement-parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let path = parent.join("thread-000.jsonl");
    let replacement_path = replacement.join("thread-000.jsonl");
    std::fs::write(&path,hydration_codex(0)).unwrap();
    std::fs::write(&replacement_path,hydration_codex(7)).unwrap();
    let paths = vec![path.clone()];
    let store = Arc::new(hydration_store());
    store.recent_codex_raw_limit.store(0,Ordering::Relaxed);
    let owner = context("compact-parent-swap");
    let root = hydration_root(&store,&source,&owner);
    let graph = hydration_graph(&store,&root,&paths,&owner);
    hydration_false(&hydration_query(&store,&graph,"definitely-absent",&owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    let old = {
        let mut state = store.lock_state();
        let facts = Arc::clone(&state.cached_prepared_facts(&old_stamp.identity).facts);
        store.retain_cached_facts(&mut state,facts)
    };
    let old_inputs = sonic_rs::to_string(&old.facts().inputs).unwrap();
    let suffix = hydration_tool("swap-positive","swap-positive");
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(suffix.as_bytes()).unwrap();
    let stamp = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    let new_key = PreparedDiskKey::new(stamp,owner["registry_generation"].as_str().unwrap(),
        "hook",&owner["authority"],&json!({"id":"native","version":"1"})).unwrap();
    let fresh = hydration_graph(&store,&root,&paths,&owner);
    let switched = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&switched);
    let opened = path.clone();
    let action: Arc<dyn Fn()+Send+Sync> = Arc::new(move || {
        std::fs::rename(&parent,&parked).unwrap();
        std::fs::rename(&replacement,&parent).unwrap();
        assert_eq!(SourceStamp::of(&std::fs::metadata(parked.join("thread-000.jsonl")).unwrap()),stamp);
        assert_ne!(SourceStamp::of(&std::fs::metadata(&opened).unwrap()),stamp);
        observed.store(true,Ordering::Release);
    });
    *store.read_hook.lock().unwrap() = Some(compact_read_countdown(Arc::downgrade(&store),4,action));
    let (answer,_) = hydration_tool_query(&store,&fresh,"swap-positive",&owner);
    assert!(switched.load(Ordering::Acquire));
    assert_eq!(answer["status"].as_str(),Some("changed"),"{answer:?}");
    assert_ne!(answer["data"]["value"].as_bool(),Some(true));
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(store.lock_state().cached_prepared_facts(&old_stamp.identity).stamp,old_stamp);
    assert_eq!(sonic_rs::to_string(&old.facts().inputs).unwrap(),old_inputs);
    store.assert_conserved();
    drop(old);
    store.assert_conserved();
}

#[test]
fn prepared_cache_compact_codex_cold_failed_call_is_restored_by_successful_result() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap()
        .write_all(compact_output("initial-0", 1).as_bytes()).unwrap();
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-cold-failed-call");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let (failed, _) = compact_error_query(&store, &graph, &owner);
    assert_eq!(failed["status"].as_str(), Some("ok"), "{failed:?}");
    assert_eq!(failed["data"]["value"].as_bool(), Some(true));
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let old = {
        let mut state = store.lock_state();
        let cached = state.cached_prepared_facts(&old_stamp.identity);
        assert!(cached.facts.inputs["calls"].as_array().unwrap().is_empty());
        assert_eq!(cached.append.as_ref().unwrap().reducer.calls.len(), 1);
        assert!(!state.latest.contains_key(&old_stamp.identity));
        assert!(!state.source_spines.contains_key(&old_stamp.identity.file()));
        let facts = Arc::clone(&cached.facts);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_bytes = sonic_rs::to_string(&old.facts().inputs).unwrap();
    let suffix = compact_output("initial-0", 0);
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap()
        .write_all(suffix.as_bytes()).unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let (answer, usage) = hydration_tool_query(&store, &fresh, "base-0", &owner);
    assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
    assert_eq!(answer["data"]["value"].as_bool(), Some(true));
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let actual = Arc::clone(&store.lock_state().cached_prepared_facts(&stamp.identity).facts);
    let complete = compact_full_oracle(&paths[0]);
    for column in ["calls", "commands", "edited_files", "skills"] {
        assert_eq!(sonic_rs::to_string(&actual.inputs[column]).unwrap(),
            sonic_rs::to_string(&complete.inputs[column]).unwrap(), "column {column}");
    }
    assert_eq!(sonic_rs::to_string(&actual.inputs).unwrap(), sonic_rs::to_string(&complete.inputs).unwrap());
    assert_eq!(actual.has_error, complete.has_error);
    assert!(!actual.has_error);
    assert_eq!(actual.inputs["calls"].as_array().unwrap().len(), 1);
    assert!(usage[1] <= suffix.len() as u64 + 256);
    assert_eq!(usage[15], 0);
    assert!(old.facts().has_error);
    assert_eq!(sonic_rs::to_string(&old.facts().inputs).unwrap(), old_bytes);
    store.assert_conserved();
    drop(old);
    store.assert_conserved();
}

#[test]
fn prepared_cache_compact_codex_reducer_tables_grow_with_all_call_metadata_charged() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let mut seed = hydration_codex(0);
    for at in 1..3 {
        let id = format!("seed-{at}");
        seed.push_str(&hydration_tool(&id, &id));
        seed.push_str(&compact_output(&id, 0));
    }
    std::fs::write(&paths[0], seed).unwrap();
    let store = hydration_store();
    store.recent_codex_raw_limit.store(0, Ordering::Relaxed);
    let owner = context("compact-reducer-growth");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let (old_calls, old_results, count) = {
        let state = store.lock_state();
        let reducer = &state.cached_prepared_facts(&old_stamp.identity).append.as_ref().unwrap().reducer;
        assert_eq!(reducer.calls.len(), 3);
        assert_eq!(reducer.results.len(), 3);
        (reducer.calls.capacity(), reducer.results.capacity(), reducer.calls.len())
    };
    let extra = old_calls.max(old_results) - count + 1;
    let mut suffix = String::new();
    for at in 0..extra {
        let id = format!("new-reducer-{at}");
        suffix.push_str(&hydration_tool(&id, &id));
        suffix.push_str(&compact_output(&id, 0));
    }
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let previous = store.previous_compact_codex(stamp, &owner, &json!({"id":"native","version":"1"})).unwrap().unwrap();
    let old_inputs = sonic_rs::to_string(&previous.append.reducer.inputs()).unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let (answer, usage) = hydration_query(&store, &fresh, "definitely-absent", &owner);
    hydration_false(&answer);
    let state = store.lock_state();
    let cached = state.cached_prepared_facts(&stamp.identity);
    let reducer = &cached.append.as_ref().unwrap().reducer;
    assert!(reducer.calls.capacity() > old_calls);
    assert!(reducer.results.capacity() > old_results);
    assert_eq!(reducer.calls.len(), count + extra);
    let complete = compact_full_oracle(&paths[0]);
    for column in ["calls", "commands", "edited_files", "skills"] {
        assert_eq!(sonic_rs::to_string(&cached.facts.inputs[column]).unwrap(),
            sonic_rs::to_string(&complete.inputs[column]).unwrap(), "column {column}");
    }
    assert_eq!(sonic_rs::to_string(&cached.facts.inputs).unwrap(), sonic_rs::to_string(&complete.inputs).unwrap());
    assert_eq!(cached.facts.has_error, complete.has_error);
    assert_eq!(sonic_rs::to_string(&previous.append.reducer.inputs()).unwrap(), old_inputs);
    eprintln!("compact_reducer_growth old_calls={old_calls} new_calls={} old_results={old_results} new_results={} suffix={} source_bytes={}",
        reducer.calls.capacity(), reducer.results.capacity(), suffix.len(), usage[1]);
    drop(state);
    assert!(usage[1] <= suffix.len() as u64 + 256);
    assert_eq!(usage[15], 0);
    store.assert_conserved();
    drop(previous);
    store.assert_conserved();
}

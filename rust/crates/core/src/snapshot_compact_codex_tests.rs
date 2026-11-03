fn compact_output(id: &str, exit_code: i64) -> String {
    format!(
        "{}\n",
        json!({"timestamp":"2026-01-02T03:04:08Z","type":"response_item","payload":{
        "type":"function_call_output","call_id":id,"output":{"metadata":{"exit_code":exit_code},"output":"result"}}})
    )
}

fn compact_error_query(store: &NativeStore, graph: &Value, owner: &Value) -> (Value, [u64; 20]) {
    let mut request = hydration_query_request(graph, "unused");
    request.insert("query", json!({"kind":"has_error","subagents":true}));
    hydration_run_query(store, &request, owner)
}

fn compact_assert_index_inputs(actual: &Value, expected: &Value) {
    let schema: BTreeSet<&str> = ["calls", "commands", "edited_files", "skills"]
        .into_iter()
        .collect();
    let actual_object = actual.as_object().expect("actual Index inputs object");
    let expected_object = expected.as_object().expect("expected Index inputs object");
    assert_eq!(actual_object.len(), schema.len());
    assert_eq!(expected_object.len(), schema.len());
    let actual_keys: BTreeSet<&str> = actual_object.iter().map(|(key, _)| key).collect();
    let expected_keys: BTreeSet<&str> = expected_object.iter().map(|(key, _)| key).collect();
    assert_eq!(actual_keys, schema);
    assert_eq!(expected_keys, schema);
    for column in ["calls", "commands", "edited_files", "skills"] {
        assert!(
            actual[column].as_array().is_some(),
            "actual column {column}"
        );
        assert!(
            expected[column].as_array().is_some(),
            "expected column {column}"
        );
        assert_eq!(
            sonic_rs::to_string(&actual[column]).unwrap(),
            sonic_rs::to_string(&expected[column]).unwrap(),
            "column {column}",
        );
    }
    assert_eq!(
        crate::ids::canonical_json(actual).unwrap(),
        crate::ids::canonical_json(expected).unwrap(),
    );
}

fn compact_full_oracle(path: &Path) -> crate::snapshot_prepared::PreparedFacts {
    let bytes = std::fs::read(path).unwrap();
    let parsed = parse_codex_bytes(&bytes);
    let entries = lower(&parsed).entries;
    let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
    let snapshot = TranscriptSnapshot::from_complete_entries(
        String::new(),
        path.to_path_buf(),
        stamp,
        Provider::Codex,
        "oracle".to_owned(),
        entries,
    );
    let bound = WorkLimits {
        max_read_bytes: 1024 * 1024,
        max_source_read_bytes: 1024 * 1024,
        max_events: 4096,
        max_items: 256,
        max_output_bytes: 1024 * 1024,
        max_discovery_entries: 4096,
        max_sources: 128,
        deadline_unix_ms: now_ms() + 30_000,
    };
    crate::snapshot_projection::prepare_facts(
        &snapshot,
        &json!([]),
        &bound,
        &Cancellation::default(),
        FactsNeed::Index,
    )
    .unwrap()
    .0
}

fn agent_hydration_store() -> NativeStore {
    let store = NativeStore::new_agent(
        &json!({"max_retained_bytes":128*1024*1024,"max_prepared_fact_memory_bytes":16*1024*1024}),
    )
    .unwrap();
    *store.hydration_observations.lock().unwrap() = Some(Vec::new());
    store
}

fn agent_continuation(store: &NativeStore, stamp: SourceStamp) -> (usize, usize) {
    let mut state = store.lock_state();
    let snapshot = state
        .latest_source(&stamp.identity)
        .expect("retained agent source");
    assert_eq!(snapshot.stamp, stamp);
    assert!(snapshot.codex_raw.is_none());
    assert!(snapshot.codex_append.is_none());
    let continuation = state
        .generations
        .get(&snapshot_key(&snapshot))
        .unwrap()
        .agent_codex
        .as_ref()
        .unwrap();
    (
        continuation.as_ref() as *const crate::codex::CodexReviewContinuation as usize,
        continuation.accounted_bytes(),
    )
}

fn append_compact_source(path: &Path, suffix: &str) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(suffix.as_bytes())
        .unwrap();
}

fn assert_agent_index_oracle(store: &NativeStore, path: &Path, error: bool) {
    let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
    let actual = Arc::clone(
        &store
            .lock_state()
            .cached_prepared_facts(&stamp.identity)
            .facts,
    );
    let complete = compact_full_oracle(path);
    compact_assert_index_inputs(&actual.index_inputs(), &complete.index_inputs());
    assert_eq!(actual.has_error(), complete.has_error());
    assert_eq!(actual.has_error(), error);
    assert!(!actual.is_full());
    agent_continuation(store, stamp);
    store.assert_conserved();
}

#[test]
fn agent_codex_late_duplicate_results_match_complete_projection() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = agent_hydration_store();
    let owner = context("agent-results");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let initial = compact_error_query(&store, &graph, &owner).0;
    hydration_false(&initial);
    let initial_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let old = {
        let mut state = store.lock_state();
        let cached = state.cached_prepared_facts(&initial_stamp.identity);
        assert!(cached.append.is_none());
        let facts = Arc::clone(&cached.facts);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_inputs = sonic_rs::to_string(&old.facts().index_inputs()).unwrap();
    let suffixes = [
        (compact_output("initial-0", 1), true),
        (compact_output("initial-0", 0), false),
        (compact_output("not-yet-called", 1), false),
        (hydration_tool("late-call", "not-yet-called"), true),
        (hydration_tool("duplicate-call", "initial-0"), true),
        (compact_output("not-yet-called", 0), false),
    ];
    for (suffix, expected) in suffixes {
        append_compact_source(&paths[0], &suffix);
        let fresh = hydration_graph(&store, &root, &paths, &owner);
        let (answer, usage) = compact_error_query(&store, &fresh, &owner);
        assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
        assert_eq!(answer["data"]["value"].as_bool(), Some(expected));
        assert_agent_index_oracle(&store, &paths[0], expected);
        assert!(usage[1] <= suffix.len() as u64 + 256);
        assert_eq!(usage[15], 0);
        assert!(!old.facts().has_error());
        assert_eq!(
            sonic_rs::to_string(&old.facts().index_inputs()).unwrap(),
            old_inputs
        );
    }
    drop(old);
    store.assert_conserved();
}

#[test]
fn agent_codex_owner_retirement_keeps_guarded_facts_and_releases_their_last_owner() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = agent_hydration_store();
    let owner = context("agent-owner");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let (weak, held) = {
        let mut state = store.lock_state();
        let facts = Arc::clone(&state.cached_prepared_facts(&stamp.identity).facts);
        let weak = Arc::downgrade(&facts);
        let held = store.retain_cached_facts(&mut state, facts);
        assert_eq!(state.remove_identity_facts(stamp.identity), 1);
        (weak, held)
    };
    assert!(weak.upgrade().is_some());
    assert!(!held.facts().has_error());
    assert!(store
        .lock_state()
        .identity_facts(stamp.identity)
        .next()
        .is_none());
    agent_continuation(&store, stamp);
    store.assert_conserved();
    drop(held);
    assert!(weak.upgrade().is_none());
    store.assert_conserved();
}

#[test]
fn agent_codex_refusal_cancel_and_full_need_keep_authority() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = agent_hydration_store();
    let owner = context("agent-refusal");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let classifier = json!({"id":"native","version":"1"});
    let old_key = PreparedDiskKey::new(
        stamp,
        owner["registry_generation"].as_str().unwrap(),
        "hook",
        &owner["authority"],
        &classifier,
    )
    .unwrap();
    append_compact_source(&paths[0], &hydration_tool("new-refusal", "new-refusal"));
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let token = Cancellation::default();
    token.cancel();
    assert_eq!(
        store.request_agent(&hydration_query_request(&fresh, "absent"), &owner, &token)["status"]
            .as_str(),
        Some("cancelled")
    );
    let mut refused = hydration_query_request(&fresh, "absent");
    refused["limits"].insert("max_source_read_bytes", json!(1));
    let refused = finish_prepared(
        &store,
        store.request_agent(&refused, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(
        refused["status"].as_str(),
        Some("incomplete"),
        "{refused:?}"
    );
    assert_ne!(refused["data"]["value"].as_bool(), Some(false));
    assert!(store.lock_state().prepared_facts.contains_key(&old_key));
    let (success, _) = hydration_tool_query(&store, &fresh, "new-refusal", &owner);
    assert_eq!(success["data"]["value"].as_bool(), Some(true));
    let mut full = hydration_query_request(&fresh, "unused");
    full.insert("query", json!({"kind":"has_override","token":"agent-override-absent-63bc","invalidated_by":[],"subagents":true}));
    let (full, usage) = hydration_run_query(&store, &full, &owner);
    assert_eq!(full["status"].as_str(), Some("ok"), "{full:?}");
    assert_eq!(full["data"]["value"].as_bool(), Some(false));
    assert_eq!(usage[15], 0);
    assert_eq!(usage[1], 0);
    store.assert_conserved();
}

#[test]
fn agent_codex_cancel_and_exact_refusal_never_publish_a_successor() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = agent_hydration_store();
    let owner = context("agent-inflight-refusal");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    append_compact_source(
        &paths[0],
        &hydration_tool("never-published", "never-published"),
    );
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let new_key = PreparedDiskKey::new(
        stamp,
        owner["registry_generation"].as_str().unwrap(),
        "hook",
        &owner["authority"],
        &json!({"id":"native","version":"1"}),
    )
    .unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let request = hydration_query_request(&fresh, "never-published");
    let cancellation = Cancellation::default();
    let trigger = cancellation.clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || trigger.cancel()));
    let failed = store.request_agent(&request, &owner, &cancellation);
    assert_eq!(failed["status"].as_str(), Some("cancelled"), "{failed:?}");
    assert!(failed["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(
        store
            .lock_state()
            .cached_prepared_facts(&stamp.identity)
            .stamp,
        old_stamp
    );
    store.assert_conserved();
    let used = {
        let mut state = store.lock_state();
        while let Some(victim) = state.next_cache_victim() {
            state.evict_cache_owner(victim);
        }
        NativeStore::retained_total(&mut state)
    };
    let hold = store
        .reserve_projection(&owner, store.memory_cap(&owner).unwrap() - used)
        .unwrap();
    let failed = store.request_agent(&request, &owner, &Cancellation::default());
    assert_eq!(
        failed["status"].as_str(),
        Some("retained_limit"),
        "{failed:?}"
    );
    assert_eq!(failed["usage"]["source_bytes_read"].as_u64(), Some(0));
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(
        store
            .lock_state()
            .cached_prepared_facts(&stamp.identity)
            .stamp,
        old_stamp
    );
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
fn agent_codex_capacity_boundary_preserves_the_unique_continuation_and_charges_growth() {
    for echo in [false, true] {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths = hydration_sources(&source, 1);
        let target = HashSet::<String>::with_capacity(1024).capacity();
        let mut raw = hydration_codex(0);
        for at in usize::from(!echo)..target {
            raw.push_str(&compact_assistant_line(&format!("capacity-{at}"), echo));
        }
        std::fs::write(&paths[0], raw).unwrap();
        let store = agent_hydration_store();
        let owner = context("agent-capacity-boundary");
        let root = hydration_root(&store, &source, &owner);
        let graph = hydration_graph(&store, &root, &paths, &owner);
        hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
        let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
        let (old_pointer, old_bytes) = agent_continuation(&store, old_stamp);
        let suffix = compact_assistant_line("capacity-new-boundary", echo);
        append_compact_source(&paths[0], &suffix);
        let fresh = hydration_graph(&store, &root, &paths, &owner);
        let (answer, usage) = hydration_index_query(&store, &fresh, "definitely-absent", &owner);
        hydration_false(&answer);
        let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
        let (pointer, bytes) = agent_continuation(&store, stamp);
        assert_eq!(pointer, old_pointer);
        assert!(bytes > old_bytes);
        assert!(usage[1] <= suffix.len() as u64 + 256);
        assert_eq!(usage[15], 0);
        assert_agent_index_oracle(&store, &paths[0], false);
    }
}

#[test]
fn agent_codex_final_seal_parent_swap_rejects_positive_and_keeps_old_facts() {
    let source = Source::new(&format!("{}\n", user("root")));
    let parent = source.directory.join("original-parent");
    let parked = source.directory.join("parked-parent");
    let replacement = source.directory.join("replacement-parent");
    std::fs::create_dir(&parent).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let path = parent.join("thread-000.jsonl");
    let replacement_path = replacement.join("thread-000.jsonl");
    std::fs::write(&path, hydration_codex(0)).unwrap();
    std::fs::write(&replacement_path, hydration_codex(7)).unwrap();
    let paths = vec![path.clone()];
    let store = agent_hydration_store();
    let owner = context("compact-parent-swap");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    let old = {
        let mut state = store.lock_state();
        let facts = Arc::clone(&state.cached_prepared_facts(&old_stamp.identity).facts);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_inputs = sonic_rs::to_string(&old.facts().index_inputs()).unwrap();
    let suffix = hydration_tool("swap-positive", "swap-positive");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(suffix.as_bytes())
        .unwrap();
    let stamp = SourceStamp::of(&std::fs::metadata(&path).unwrap());
    let new_key = PreparedDiskKey::new(
        stamp,
        owner["registry_generation"].as_str().unwrap(),
        "hook",
        &owner["authority"],
        &json!({"id":"native","version":"1"}),
    )
    .unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let switched = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&switched);
    let opened = path.clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::rename(&parent, &parked).unwrap();
        std::fs::rename(&replacement, &parent).unwrap();
        assert_eq!(
            SourceStamp::of(&std::fs::metadata(parked.join("thread-000.jsonl")).unwrap()),
            stamp
        );
        assert_ne!(SourceStamp::of(&std::fs::metadata(&opened).unwrap()), stamp);
        observed.store(true, Ordering::Release);
    }));
    let (answer, _) = hydration_tool_query(&store, &fresh, "swap-positive", &owner);
    assert!(switched.load(Ordering::Acquire));
    assert_eq!(answer["status"].as_str(), Some("changed"), "{answer:?}");
    assert_ne!(answer["data"]["value"].as_bool(), Some(true));
    assert!(!store.lock_state().prepared_facts.contains_key(&new_key));
    assert_eq!(
        store
            .lock_state()
            .cached_prepared_facts(&old_stamp.identity)
            .stamp,
        old_stamp
    );
    assert_eq!(
        sonic_rs::to_string(&old.facts().index_inputs()).unwrap(),
        old_inputs
    );
    store.assert_conserved();
    drop(old);
    store.assert_conserved();
}

#[test]
fn agent_codex_cold_failed_call_is_restored_by_a_successful_result() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    append_compact_source(&paths[0], &compact_output("initial-0", 1));
    let store = agent_hydration_store();
    let owner = context("agent-cold-failed-call");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let (failed, _) = compact_error_query(&store, &graph, &owner);
    assert_eq!(failed["status"].as_str(), Some("ok"), "{failed:?}");
    assert_eq!(failed["data"]["value"].as_bool(), Some(true));
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let old = {
        let mut state = store.lock_state();
        let cached = state.cached_prepared_facts(&old_stamp.identity);
        assert!(cached.facts.index_inputs()["calls"]
            .as_array()
            .unwrap()
            .is_empty());
        let facts = Arc::clone(&cached.facts);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_bytes = sonic_rs::to_string(&old.facts().index_inputs()).unwrap();
    let suffix = compact_output("initial-0", 0);
    append_compact_source(&paths[0], &suffix);
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let (answer, usage) = hydration_tool_query(&store, &fresh, "base-0", &owner);
    assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
    assert_eq!(answer["data"]["value"].as_bool(), Some(true));
    assert_agent_index_oracle(&store, &paths[0], false);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    assert_eq!(
        store
            .lock_state()
            .cached_prepared_facts(&stamp.identity)
            .facts
            .index_inputs()["calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(usage[1] <= suffix.len() as u64 + 256);
    assert_eq!(usage[15], 0);
    assert!(old.facts().has_error());
    assert_eq!(
        sonic_rs::to_string(&old.facts().index_inputs()).unwrap(),
        old_bytes
    );
    drop(old);
    store.assert_conserved();
}

#[test]
fn agent_codex_call_and_result_growth_preserves_all_columns_and_guarded_old_facts() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let mut seed = hydration_codex(0);
    for at in 1..3 {
        let id = format!("seed-{at}");
        seed.push_str(&hydration_tool(&id, &id));
        seed.push_str(&compact_output(&id, 0));
    }
    std::fs::write(&paths[0], seed).unwrap();
    let store = agent_hydration_store();
    let owner = context("agent-call-result-growth");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_index_query(&store, &graph, "definitely-absent", &owner).0);
    let old_stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let old = {
        let mut state = store.lock_state();
        let facts = Arc::clone(&state.cached_prepared_facts(&old_stamp.identity).facts);
        assert_eq!(facts.index_inputs()["calls"].as_array().unwrap().len(), 3);
        store.retain_cached_facts(&mut state, facts)
    };
    let old_inputs = old.facts().index_inputs();
    let (old_pointer, old_bytes) = agent_continuation(&store, old_stamp);
    let mut suffix = String::new();
    for at in 0..32 {
        let id = format!("new-growth-{at}");
        suffix.push_str(&hydration_tool(&id, &id));
        suffix.push_str(&compact_output(&id, 0));
    }
    append_compact_source(&paths[0], &suffix);
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    let (answer, usage) = hydration_index_query(&store, &fresh, "definitely-absent", &owner);
    hydration_false(&answer);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let (pointer, bytes) = agent_continuation(&store, stamp);
    assert_eq!(pointer, old_pointer);
    assert!(bytes > old_bytes);
    {
        let mut state = store.lock_state();
        assert_eq!(state.identity_facts(stamp.identity).count(), 1);
        assert_eq!(
            state
                .cached_prepared_facts(&stamp.identity)
                .facts
                .index_inputs()["calls"]
                .as_array()
                .unwrap()
                .len(),
            35
        );
    }
    assert_agent_index_oracle(&store, &paths[0], false);
    compact_assert_index_inputs(&old.facts().index_inputs(), &old_inputs);
    assert!(usage[1] <= suffix.len() as u64 + 256);
    assert_eq!(usage[15], 0);
    drop(old);
    store.assert_conserved();
}

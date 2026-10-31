fn hydration_codex(index: usize) -> String {
    let metadata = json!({"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":format!("thread-{index:03}"),"cwd":"/tmp"}});
    let model = json!({"timestamp":"2026-01-02T03:04:05Z","type":"turn_context","payload":{"model":"test"}});
    let message = json!({"timestamp":"2026-01-02T03:04:06Z","type":"event_msg","payload":{"type":"agent_message","message":"x".repeat(4096)}});
    let output = json!({"timestamp":"2026-01-02T03:04:07Z","type":"response_item","payload":{"type":"function_call_output","call_id":format!("initial-{index}"),"output":"ok"}});
    format!("{metadata}\n{model}\n{message}\n{}{output}\n", hydration_tool(&format!("base-{index}"), &format!("initial-{index}")))
}

fn hydration_tool(name: &str, call: &str) -> String {
    let arguments = json!({"value":call}).to_string();
    let tool = json!({"timestamp":"2026-01-02T03:04:07Z","type":"response_item","payload":{"type":"function_call","call_id":call,"name":name,"arguments":arguments}});
    format!("{tool}\n")
}

fn hydration_store() -> NativeStore {
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,"max_prepared_fact_memory_bytes":16*1024*1024})).unwrap();
    *store.hydration_observations.lock().unwrap() = Some(Vec::new());
    assert_eq!(store.recent_codex_raw_bound(), 128*1024*1024);
    store
}

fn hydration_observations(store: &NativeStore) -> Vec<HydrationObservation> {
    std::mem::take(store.hydration_observations.lock().unwrap().as_mut().unwrap())
}

fn hydration_sources(root: &Source, count: usize) -> Vec<PathBuf> {
    (0..count).map(|index| {
        let path = root.directory.join(format!("thread-{index:03}.jsonl"));
        std::fs::write(&path, hydration_codex(index)).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 64*1024);
        path
    }).collect()
}

fn hydration_limits() -> Value {
    json!({"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":4096,
        "max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":4096,"max_sources":128})
}

fn hydration_root(store: &NativeStore, source: &Source, owner: &Value) -> Value {
    let mut request = acquire(&source.path);
    request.insert("limits", hydration_limits());
    finish_prepared(store, store.request(&request, owner, &Cancellation::default()), owner)
}

fn hydration_graph(store: &NativeStore, root: &Value, paths: &[PathBuf], owner: &Value) -> Value {
    let direct: Vec<_> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
    let request = json!({"schema":SCHEMA,"id":"hydration-prepare","operation":"prepare_graph",
        "view":{"handle":handle(root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
        "thread_ids":[],"roots":[],"direct_paths":direct,"deadline_unix_ms":now_ms()+30_000,"limits":hydration_limits()});
    let response = finish_prepared(store, store.request(&request, owner, &Cancellation::default()), owner);
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    let graph = response["data"]["handle"]["graph_id"].as_str().unwrap();
    assert_eq!(store.lock_state().prepared_graphs[graph].lock().unwrap().sources.len(), paths.len());
    response
}

fn hydration_query_request(graph: &Value, skill: &str) -> Value {
    json!({"schema":SCHEMA,"id":"hydration-query","operation":"query_graph","handle":graph["data"]["handle"],
        "selectors":[],"query":{"kind":"has_skill","values":[skill],"subagents":true},
        "deadline_unix_ms":now_ms()+30_000,"limits":hydration_limits()})
}

fn hydration_run_query(store: &NativeStore, request: &Value, owner: &Value) -> (Value, [u64; 20]) {
    let before = store.lock_state().counters;
    let response = finish_prepared(store, store.request(request, owner, &Cancellation::default()), owner);
    let after = store.lock_state().counters;
    (response, std::array::from_fn(|index| after[index] - before[index]))
}

fn hydration_query(store: &NativeStore, graph: &Value, skill: &str, owner: &Value) -> (Value, [u64; 20]) {
    hydration_run_query(store, &hydration_query_request(graph, skill), owner)
}

fn hydration_tool_query(store: &NativeStore, graph: &Value, pattern: &str, owner: &Value) -> (Value, [u64; 20]) {
    let mut request = hydration_query_request(graph, pattern);
    request.insert("query", json!({"kind":"has_tool","pattern":pattern,"subagents":true}));
    hydration_run_query(store, &request, owner)
}

fn hydration_false(response: &Value) {
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert_eq!(response["complete"].as_bool(), Some(true));
    assert_eq!(response["data"]["value"].as_bool(), Some(false));
}

fn hydration_identities(paths: &[PathBuf]) -> HashSet<SourceIdentity> {
    paths.iter().map(|path| SourceStamp::of(&std::fs::metadata(path).unwrap()).identity).collect()
}

fn hydration_fact_visits(observations: &[HydrationObservation], owner: &Value) -> HashSet<SourceIdentity> {
    observations.iter().filter_map(|observation| match observation {
        HydrationObservation::Facts { stamp, key, need: FactsNeed::Index, disposition: "request" } => {
            let expected = PreparedDiskKey::new(*stamp, owner["registry_generation"].as_str().unwrap(), owner["admission"].as_str().unwrap(), &owner["authority"], &json!({"id":"native","version":"1"})).unwrap();
            assert_eq!(*key, expected);
            Some(stamp.identity)
        },
        _ => None,
    }).collect()
}

fn hydration_compact_facts(store: &NativeStore, paths: &[PathBuf]) {
    let state = store.lock_state();
    for (index, path) in paths.iter().enumerate() {
        let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
        let cached = state.cached_prepared_facts(&stamp.identity);
        assert_eq!(cached.stamp, stamp);
        assert_eq!(cached.facts.query(&json!({"kind":"has_tool","pattern":format!("base-{index}")})).unwrap()["value"].as_bool(), Some(true));
    }
    state.assert_cache_owners();
}

#[test]
fn prepared_cache_hydration_63_cold_and_unchanged_queries_are_complete() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 63);
    let identities = hydration_identities(&paths);
    assert_eq!(identities.len(), 63);
    let store = hydration_store();
    let owner = context("hydration-unchanged");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let (cold, cold_usage) = hydration_query(&store, &graph, "definitely-absent", &owner);
    hydration_false(&cold);
    let cold_observed = hydration_observations(&store);
    assert_eq!(hydration_fact_visits(&cold_observed, &owner), identities);
    let lowered: HashSet<_> = cold_observed.iter().filter_map(|item| match item {
        HydrationObservation::Codex { stamp, fallback: Some("cold"), .. } => Some(stamp.identity),
        _ => None,
    }).collect();
    assert_eq!(lowered, identities);
    assert_eq!(cold_usage[15], 63);
    assert!(cold_usage[1] >= paths.iter().map(|path| std::fs::metadata(path).unwrap().len()).sum::<u64>());
    hydration_compact_facts(&store, &paths);
    for fresh in [false, true] {
        let repeated = if fresh { hydration_graph(&store, &root, &paths, &owner) } else { graph.clone() };
        hydration_observations(&store);
        let (answer, usage) = hydration_query(&store, &repeated, "definitely-absent", &owner);
        hydration_false(&answer);
        let observed = hydration_observations(&store);
        assert_eq!(hydration_fact_visits(&observed, &owner), identities);
        eprintln!("hydration_unchanged fresh_graph={fresh} cold_bytes={} cold_lowerings={} repeat_bytes={} repeat_lowerings={}",cold_usage[1],cold_usage[15],usage[1],usage[15]);
        assert_eq!((usage[1], usage[15], usage[16], usage[19]), (0, 0, 0, 0));
        assert!(usage[7] >= 63);
        assert!(observed.iter().all(|item| !matches!(item, HydrationObservation::Codex { .. } | HydrationObservation::Read { .. })));
        store.assert_conserved();
    }
}

#[test]
fn prepared_cache_hydration_63_pressure_append_preserves_other_sources() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 63);
    let identities = hydration_identities(&paths);
    assert_eq!(identities.len(), 63);
    let stamps: HashMap<_, _> = paths.iter().map(|path| {
        let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
        (stamp.identity, stamp)
    }).collect();
    let store = hydration_store();
    let total_raw: usize = paths.iter().map(|path| std::fs::metadata(path).unwrap().len() as usize).sum();
    let raw_limit = total_raw / 8;
    store.recent_codex_raw_limit.store(raw_limit, Ordering::Relaxed);
    let owner = context("hydration-pressure");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let (cold, _) = hydration_query(&store, &graph, "definitely-absent", &owner);
    hydration_false(&cold);
    let observed = hydration_observations(&store);
    assert_eq!(hydration_fact_visits(&observed, &owner), identities);
    let victims: Vec<_> = observed.iter().filter_map(|item| match item {
        HydrationObservation::RawVictim { identity, before_bytes, after_bytes, latest_before, spine_before, latest_after, spine_after } => {
            assert!(*before_bytes > raw_limit && after_bytes < before_bytes);
            assert!(*latest_before && *spine_before && !*latest_after && !*spine_after);
            Some(*identity)
        }
        _ => None,
    }).collect();
    assert!(!victims.is_empty());
    let victim = victims[0];
    let changed = paths.iter().find(|path| SourceStamp::of(&std::fs::metadata(path).unwrap()).identity == victim).unwrap();
    {
        let state = store.lock_state();
        assert!(!state.recent_codex.contains_key(&victim));
        assert!(!state.latest.contains_key(&victim));
        assert!(!state.source_spines.contains_key(&victim.file()));
        assert!(state.recent_codex_raw_bytes <= raw_limit);
    }
    hydration_compact_facts(&store, &paths);
    let (unchanged, unchanged_usage) = hydration_query(&store, &graph, "definitely-absent", &owner);
    hydration_false(&unchanged);
    assert_eq!((unchanged_usage[1], unchanged_usage[15], unchanged_usage[19]), (0, 0, 0));
    assert!(unchanged_usage[7] >= 63);
    let suffix = hydration_tool("new-tool", "new-call");
    std::fs::OpenOptions::new().append(true).open(changed).unwrap().write_all(suffix.as_bytes()).unwrap();
    for path in &paths {
        let stamp = SourceStamp::of(&std::fs::metadata(path).unwrap());
        if stamp.identity != victim { assert_eq!(stamps[&stamp.identity], stamp); }
    }
    let (stale, _) = hydration_query(&store, &graph, "definitely-absent", &owner);
    assert_eq!(stale["status"].as_str(), Some("changed"), "{stale:?}");
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let (answer, usage) = hydration_query(&store, &fresh, "definitely-absent", &owner);
    hydration_false(&answer);
    let after = hydration_observations(&store);
    assert_eq!(hydration_fact_visits(&after, &owner), identities);
    let read: HashSet<_> = after.iter().filter_map(|item| match item {
        HydrationObservation::Read { stamp, bytes } if *bytes > 0 => Some(stamp.identity),
        _ => None,
    }).collect();
    assert_eq!(read, HashSet::from([victim]));
    let traced_bytes: u64 = after.iter().filter_map(|item| match item {
        HydrationObservation::Read { bytes, .. } => Some(*bytes),
        _ => None,
    }).sum();
    assert_eq!(traced_bytes, usage[1]);
    let codex: Vec<_> = after.iter().filter(|item| matches!(item, HydrationObservation::Codex { stamp, .. } if stamp.identity == victim)).collect();
    assert_eq!(codex.len(), 1);
    hydration_compact_facts(&store, &paths);
    let (positive, positive_usage) = hydration_tool_query(&store, &fresh, "new-tool", &owner);
    assert_eq!(positive["status"].as_str(), Some("ok"), "{positive:?}");
    assert_eq!(positive["data"]["value"].as_bool(), Some(true));
    assert_eq!((positive_usage[1], positive_usage[15]), (0, 0));
    eprintln!("hydration_pressure_append sources=63 victims={} raw_limit={raw_limit} suffix_bytes={} source_bytes={} lowerings={} lowered_bytes={} codex={codex:?}", victims.len(), suffix.len(), usage[1], usage[15], usage[16]);
    store.assert_conserved();
    assert!(usage[1] <= suffix.len() as u64 + 256, "{after:?}");
    assert_eq!(usage[15], 0, "{after:?}");
}

#[test]
fn prepared_cache_hydration_legal_append_without_pressure_is_incremental() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("hydration-legal-append");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let prior = store.lock_state().latest_source(&stamp.identity).unwrap();
    let prior_entries = sonic_rs::to_string(&prior.entries()).unwrap();
    let suffix = hydration_tool("new-tool", "new-call");
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let fresh = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let (answer, usage) = hydration_query(&store, &fresh, "definitely-absent", &owner);
    hydration_false(&answer);
    let observed = hydration_observations(&store);
    assert!(observed.iter().any(|item| matches!(item, HydrationObservation::Codex { previous: true, raw: true, index: true, append: true, fallback: None, suffix_bytes, .. } if *suffix_bytes == suffix.len())));
    let (positive, _) = hydration_tool_query(&store, &fresh, "new-tool", &owner);
    assert_eq!(positive["data"]["value"].as_bool(), Some(true));
    eprintln!("hydration_legal_append suffix_bytes={} source_bytes={} lowerings={}",suffix.len(),usage[1],usage[15]);
    assert!(usage[1] <= suffix.len() as u64 + 256);
    assert_eq!(usage[15], 0);
    assert_eq!(sonic_rs::to_string(&prior.entries()).unwrap(), prior_entries);
    assert_eq!(prior.stamp, stamp);
    drop(prior);
    store.assert_conserved();
}

#[test]
fn prepared_cache_hydration_context_fences_and_cancellation_remain_authoritative() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,
        "max_prepared_fact_memory_bytes":16*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
    *store.hydration_observations.lock().unwrap() = Some(Vec::new());
    assert_eq!(store.recent_codex_raw_bound(), 128*1024*1024);
    let owner = context("hydration-context");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let classifier = json!({"id":"native","version":"1"});
    let original = PreparedDiskKey::new(stamp, owner["registry_generation"].as_str().unwrap(), "hook", &owner["authority"], &classifier).unwrap();
    let mut registry = owner.clone();
    registry.insert("registry_generation", json!(register_changed_registry(&store)));
    let mut admission = owner.clone();
    admission.insert("admission", json!("review"));
    let review_cap = store.memory_cap(&admission).unwrap();
    let retained = NativeStore::retained_total(&mut store.lock_state());
    assert!(retained.saturating_add(2 * MAX_REPLY_BYTES) <= review_cap);
    let mut authority = owner.clone();
    authority.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory.to_string_lossy().as_ref()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    for changed in [registry, admission, authority] {
        let different = PreparedDiskKey::new(stamp, changed["registry_generation"].as_str().unwrap(), changed["admission"].as_str().unwrap(), &changed["authority"], &classifier).unwrap();
        assert_ne!(original, different);
        let (response, usage) = hydration_query(&store, &graph, "definitely-absent", &changed);
        assert_eq!(response["status"].as_str(), Some("stale_handle"), "{response:?}");
        assert_eq!((usage[1], usage[15]), (0, 0));
    }
    let token = Cancellation::default();
    token.cancel();
    let cancelled = store.request(&hydration_query_request(&graph, "definitely-absent"), &owner, &token);
    assert_eq!(cancelled["status"].as_str(), Some("cancelled"));
    hydration_false(&hydration_query(&store, &graph, "definitely-absent", &owner).0);
    store.assert_conserved();
}

#[test]
fn prepared_cache_hydration_rewrite_replace_and_provisional_completion_keep_semantics() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("hydration-mutation");
    let root = hydration_root(&store, &source, &owner);
    let first = hydration_graph(&store, &root, &paths, &owner);
    assert_eq!(hydration_tool_query(&store, &first, "base-0", &owner).0["data"]["value"].as_bool(), Some(true));
    let original = hydration_codex(0);
    let rewritten = original.replace("base-0", "gone-0");
    assert_eq!(rewritten.len(), original.len());
    std::fs::write(&paths[0], &rewritten).unwrap();
    assert_eq!(hydration_tool_query(&store, &first, "base-0", &owner).0["status"].as_str(), Some("changed"));
    let rewritten_graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_tool_query(&store, &rewritten_graph, "base-0", &owner).0);
    assert_eq!(hydration_tool_query(&store, &rewritten_graph, "gone-0", &owner).0["data"]["value"].as_bool(), Some(true));
    let truncated = original.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(&paths[0], truncated).unwrap();
    assert_eq!(hydration_tool_query(&store, &rewritten_graph, "gone-0", &owner).0["status"].as_str(), Some("changed"));
    let truncated_graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_tool_query(&store, &truncated_graph, "gone-0", &owner).0);
    let replacement = source.directory.join("replacement.jsonl");
    std::fs::write(&replacement, original.trim_end_matches('\n')).unwrap();
    std::fs::rename(replacement, &paths[0]).unwrap();
    assert_eq!(hydration_tool_query(&store, &truncated_graph, "base-0", &owner).0["status"].as_str(), Some("changed"));
    let provisional = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(&hydration_query(&store, &provisional, "definitely-absent", &owner).0);
    let suffix = "\n".to_owned() + &hydration_tool("completed-tool", "completed-call");
    std::fs::OpenOptions::new().append(true).open(&paths[0]).unwrap().write_all(suffix.as_bytes()).unwrap();
    let completed = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    assert_eq!(hydration_tool_query(&store, &completed, "completed-tool", &owner).0["data"]["value"].as_bool(), Some(true));
    let observed = hydration_observations(&store);
    assert!(observed.iter().any(|item| matches!(item, HydrationObservation::Codex { fallback: Some("partial_tail"), .. })));
    store.assert_conserved();
}

#[test]
fn prepared_cache_hydration_source_budget_refusal_keeps_incomplete_evidence() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("hydration-refusal");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let mut request = hydration_query_request(&graph, "definitely-absent");
    request["limits"].insert("max_source_read_bytes", json!(1));
    let refused = finish_prepared(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(refused["status"].as_str(), Some("incomplete"), "{refused:?}");
    assert_eq!(refused["reason"].as_str(), Some("source_read_limit"));
    assert!(refused["cursor"].is_null());
    assert_ne!(refused["data"]["value"].as_bool(), Some(false));
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    assert!(!store.lock_state().has_prepared_facts(&stamp.identity));
    store.assert_conserved();
}

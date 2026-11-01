#[test]
fn typed_skills_cold_codex_has_no_generic_lowering() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 63);
    let old_payload = "x".repeat(4096);
    let payload = "x".repeat(2 * 1024 * 1024);
    for (index, path) in paths.iter().enumerate() {
        std::fs::write(path, hydration_codex(index).replace(&old_payload, &payload)).unwrap();
    }
    let store = NativeStore::new(&json!({"max_retained_bytes":512*1024*1024,
        "max_prepared_fact_memory_bytes":16*1024*1024}))
    .unwrap();
    *store.hydration_observations.lock().unwrap() = Some(Vec::new());
    let owner = context("typed-skills-cold");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let mut request = hydration_query_request(&graph, "absent");
    request["limits"].insert("max_read_bytes", json!(256 * 1024 * 1024));
    request["limits"].insert("max_source_read_bytes", json!(256 * 1024 * 1024));
    let (answer, usage) = hydration_run_query(&store, &request, &owner);
    hydration_false(&answer);
    let observed = hydration_observations(&store);
    eprintln!(
        "TYPED_SKILLS_COST sources={} source_bytes={} read_bytes={} decoded_bytes={} lowerings={}",
        paths.len(),
        paths
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .sum::<u64>(),
        usage[1],
        usage[2],
        usage[15]
    );
    assert!(
        !observed
            .iter()
            .any(|item| matches!(item, HydrationObservation::Codex { .. })),
        "TYPED_SKILLS_NO_CODEX_LOWERING"
    );
    assert_eq!(usage[15], 0, "TYPED_SKILLS_NO_CODEX_LOWERING");
    let bytes: u64 = paths
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum();
    assert_eq!(usage[1], bytes + 128 * paths.len() as u64);
    let header_bytes: u64 = (0..paths.len())
        .map(|index| hydration_codex(index).lines().next().unwrap().len() as u64)
        .sum();
    assert_eq!(usage[2], header_bytes);
    let (again, repeated) = hydration_run_query(&store, &request, &owner);
    hydration_false(&again);
    assert_eq!(repeated[1], 0);
    assert_eq!(repeated[15], 0);
    store.lock_state().assert_cache_owners();
}

#[test]
fn typed_skills_capability_does_not_answer_tool_queries() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-capability");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let request = hydration_query_request(&graph, "absent");
    hydration_false(&hydration_run_query(&store, &request, &owner).0);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    {
        let state = store.lock_state();
        let facts = &state.cached_prepared_facts(&stamp.identity).facts;
        assert!(
            facts
                .query(&json!({"kind":"has_tool","pattern":"base-0"}))
                .is_err(),
            "TYPED_SKILLS_CAPABILITY_NO_INDEX"
        );
    }
    let (tool, _) = hydration_tool_query(&store, &graph, "base-0", &owner);
    assert_eq!(tool["status"].as_str(), Some("ok"));
    assert_eq!(tool["data"]["value"].as_bool(), Some(true));
}

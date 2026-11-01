fn skill_claude(name: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":"skill-entry","sessionId":"skill-session","timestamp":"2026-01-02T03:04:07Z",
        "message":{"role":"assistant","model":"test","content":[{"type":"tool_use","id":"skill-call","name":"Skill","input":{"skill":name}}]}})
    )
}

#[test]
fn typed_skills_mixed_provider_child_and_tool_observer_remain_visible() {
    let source = Source::new(&hydration_codex(900));
    let mut paths = hydration_sources(&source, 1);
    let child = source.directory.join("thread-claimed-codex.jsonl");
    std::fs::write(&child, skill_claude("plugin:target")).unwrap();
    paths.push(child);
    let store = hydration_store();
    let owner = context("typed-skills-mixed");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let request = hydration_query_request(&graph, "plugin:target");
    let (answer, _) = hydration_run_query(&store, &request, &owner);
    assert_eq!(answer["status"].as_str(), Some("ok"));
    assert_eq!(answer["data"]["value"].as_bool(), Some(true));
    let mut skills = hydration_query_request(&graph, "absent");
    skills.insert(
        "observers",
        json!([{"kind":"has_skill_suffix","values":["target"]}]),
    );
    let (observed, usage) = hydration_run_query(&store, &skills, &owner);
    hydration_false(&observed);
    assert_eq!(observed["data"]["observers"][0].as_bool(), Some(true));
    assert_eq!(usage[1], 0);
    let mut observer = hydration_query_request(&graph, "absent");
    observer.insert("observers", json!([{"kind":"has_tool","pattern":"base-0"}]));
    let (answer, _) = hydration_run_query(&store, &observer, &owner);
    hydration_false(&answer);
    assert_eq!(answer["data"]["observers"][0].as_bool(), Some(true));
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let state = store.lock_state();
    assert!(state
        .cached_prepared_facts(&stamp.identity)
        .facts
        .serves(FactsNeed::Index));
    assert_eq!(
        state
            .cached_prepared_facts(&stamp.identity)
            .facts
            .query(&json!({"kind":"has_tool","pattern":"base-0"}))
            .unwrap()["value"]
            .as_bool(),
        Some(true)
    );
}

#[test]
fn typed_skills_append_uses_fenced_suffix_without_lowering() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-append");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(
        &hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner).0,
    );
    let suffix = hydration_tool("after", "append");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&paths[0])
        .unwrap()
        .write_all(suffix.as_bytes())
        .unwrap();
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_observations(&store);
    let (answer, usage) =
        hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner);
    hydration_false(&answer);
    assert_eq!(usage[1], suffix.len() as u64 + 192);
    assert_eq!(usage[15], 0);
    assert!(!hydration_observations(&store)
        .iter()
        .any(|item| matches!(item, HydrationObservation::Codex { .. })));
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let state = store.lock_state();
    assert!(matches!(
        state.cached_prepared_facts(&stamp.identity).facts.evidence,
        crate::snapshot_prepared::PreparedEvidence::Skills {
            evidence: crate::snapshot_prepared::SkillEvidence::CodexFramed { .. }
        }
    ));
}

#[test]
fn typed_skills_hard_entry_bounds_include_later_blank_and_unterminated_lines() {
    for tail in [format!("{}\n", " ".repeat(257)), "x".repeat(257)] {
        let source = Source::new(&format!("{}\n", user("root")));
        let child = source.directory.join("codex-child.jsonl");
        let header = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"id","cwd":"/tmp"}})
        );
        std::fs::write(&child, header + &tail).unwrap();
        let store = NativeStore::new(&json!({"max_entry_bytes":256,"max_read_bytes_per_step":64,"max_retained_bytes":128*1024*1024})).unwrap();
        let owner = context("typed-skills-entry");
        let root = hydration_root(&store, &source, &owner);
        let graph = hydration_graph(&store, &root, &[child], &owner);
        let (answer, _) =
            hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner);
        assert_eq!(answer["status"].as_str(), Some("entry_limit"), "{answer:?}");
        store.lock_state().assert_cache_owners();
    }
}

#[test]
fn typed_skills_provisional_append_carries_the_entire_line_length() {
    let source = Source::new(&format!("{}\n", user("root")));
    let child = source.directory.join("provisional.jsonl");
    let header = format!(
        "{}\n",
        json!({"type":"session_meta","payload":{"id":"id","cwd":"/tmp"}})
    );
    std::fs::write(&child, header + &"x".repeat(400)).unwrap();
    let store = NativeStore::new(&json!({"max_entry_bytes":512,"max_read_bytes_per_step":64,"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("typed-skills-provisional");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &[child.clone()], &owner);
    hydration_false(
        &hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner).0,
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&child)
        .unwrap()
        .write_all("x".repeat(113).as_bytes())
        .unwrap();
    let graph = hydration_graph(&store, &root, &[child], &owner);
    let (answer, _) =
        hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner);
    assert_eq!(answer["status"].as_str(), Some("entry_limit"), "{answer:?}");
    store.lock_state().assert_cache_owners();
}

#[test]
fn typed_skills_full_observer_still_requires_full_native_evidence() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-full");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let mut request = hydration_query_request(&graph, "absent");
    request.insert(
        "observers",
        json!([{"kind":"has_override","token":"missing-override","invalidated_by":[]}]),
    );
    let (answer, usage) = hydration_run_query(&store, &request, &owner);
    hydration_false(&answer);
    assert_eq!(answer["data"]["observers"][0].as_bool(), Some(false));
    assert_eq!(usage[15], 1);
    let stamp = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap());
    let state = store.lock_state();
    assert!(state
        .cached_prepared_facts(&stamp.identity)
        .facts
        .serves(FactsNeed::Full));
}

#[test]
fn typed_skills_read_cancellation_does_not_publish_or_park_partial_evidence() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-cancel");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let cancel = Cancellation::default();
    let interrupted = cancel.clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || interrupted.cancel()));
    let answer = store.request(&hydration_query_request(&graph, "absent"), &owner, &cancel);
    assert_eq!(answer["status"].as_str(), Some("cancelled"), "{answer:?}");
    let identity = SourceStamp::of(&std::fs::metadata(&paths[0]).unwrap()).identity;
    let state = store.lock_state();
    assert_eq!(state.identity_facts(identity).count(), 0);
    assert!(state.prepared_queries.is_empty());
    state.assert_cache_owners();
}

#[test]
#[cfg(target_os = "linux")]
fn typed_skills_parked_fd_and_bytes_drop_on_release_and_prune() {
    use std::os::fd::AsRawFd;
    for prune in [false, true] {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths = hydration_sources(&source, 1);
        let store = NativeStore::new(
            &json!({"max_read_bytes_per_step":64,"max_retained_bytes":128*1024*1024}),
        )
        .unwrap();
        let owner = context("typed-skills-parked");
        let root = hydration_root(&store, &source, &owner);
        let graph = hydration_graph(&store, &root, &paths, &owner);
        let first = store.request(
            &hydration_query_request(&graph, "absent"),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        let (token, fd) = {
            let state = store.lock_state();
            assert_eq!(state.prepared_queries.len(), 1);
            state.assert_cache_owners();
            let (token, cursor) = state.prepared_queries.iter().next().unwrap();
            let Some(PendingQuerySource::Skills(work)) = &cursor.pending else {
                panic!("Skills work must be parked")
            };
            (token.clone(), work.file.as_raw_fd())
        };
        let descriptor = PathBuf::from(format!("/proc/self/fd/{fd}"));
        let canonical = std::fs::canonicalize(&paths[0]).unwrap();
        assert_eq!(std::fs::read_link(&descriptor).unwrap(), canonical);
        if prune {
            let mut state = store.lock_state();
            state.prepared_queries.get_mut(&token).unwrap().expires = now_ms().saturating_sub(1);
            NativeStore::prune(&mut state);
        } else {
            assert!(store.release_cursor(&token, &owner).unwrap());
        }
        assert!(!std::fs::read_link(&descriptor).is_ok_and(|path| path == canonical));
        let state = store.lock_state();
        assert!(state.prepared_queries.is_empty());
        state.assert_cache_owners();
    }
}

#[test]
fn typed_skills_active_and_parked_admission_preserves_foreground_headroom() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = NativeStore::new(&json!({"max_pending_loads":2,"reserved_hook_loads":1,
        "reserved_hook_accounted_bytes":4096,"max_read_bytes_per_step":64,
        "max_retained_bytes":128*1024*1024}))
    .unwrap();
    let mut owner = context("typed-skills-admission");
    owner.insert("admission", json!("hook"));
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let request = hydration_query_request(&graph, "absent");
    let mut background = owner.clone();
    background.insert("work_class", json!("background"));
    let first = store.request(&request, &background, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    let opens = store.lock_state().counters[0];
    let refused = store.request(&request, &background, &Cancellation::default());
    assert_eq!(store.lock_state().counters[0], opens);
    assert_eq!(
        refused["status"].as_str(),
        Some("retained_limit"),
        "{refused:?}"
    );
    assert_eq!(
        store.lock_state().skill_admissions.load(Ordering::Acquire),
        1
    );
    let foreground = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(
        foreground["status"].as_str(),
        Some("incomplete"),
        "{foreground:?}"
    );
    let opens = store.lock_state().counters[0];
    let refused = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(store.lock_state().counters[0], opens);
    assert_eq!(
        refused["status"].as_str(),
        Some("retained_limit"),
        "{refused:?}"
    );
    let tokens = {
        let state = store.lock_state();
        assert_eq!(state.skill_admissions.load(Ordering::Acquire), 2);
        assert_eq!(Arc::strong_count(&state.skill_admissions), 3);
        state.assert_cache_owners();
        state
            .prepared_queries
            .iter()
            .map(|(token, _)| token.clone())
            .collect::<Vec<_>>()
    };
    for token in tokens {
        assert!(store.release_cursor(&token, &owner).unwrap());
    }
    let state = store.lock_state();
    assert_eq!(state.skill_admissions.load(Ordering::Acquire), 0);
    assert_eq!(Arc::strong_count(&state.skill_admissions), 1);
    assert_eq!(arc_bytes::<AtomicUsize>(), arc_mirror::<AtomicUsize>());
    state.assert_cache_owners();
}

#[test]
fn typed_skills_empty_source_can_gain_its_first_codex_provider() {
    let source = Source::new(&format!("{}\n", user("root")));
    let child = source.directory.join("empty.jsonl");
    std::fs::write(&child, " \n\n").unwrap();
    let store = hydration_store();
    let owner = context("typed-skills-empty");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &[child.clone()], &owner);
    hydration_false(
        &hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner).0,
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&child)
        .unwrap()
        .write_all(hydration_codex(0).as_bytes())
        .unwrap();
    let graph = hydration_graph(&store, &root, &[child.clone()], &owner);
    hydration_observations(&store);
    let (answer, usage) =
        hydration_run_query(&store, &hydration_query_request(&graph, "absent"), &owner);
    hydration_false(&answer);
    assert_eq!(usage[1], std::fs::metadata(&child).unwrap().len() + 128);
    assert_eq!(usage[15], 0);
}

#[test]
fn typed_skills_same_inode_rewrite_reclassifies_the_current_provider() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-rewrite");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    hydration_false(
        &hydration_run_query(&store, &hydration_query_request(&graph, "target"), &owner).0,
    );
    let old = std::fs::metadata(&paths[0]).unwrap();
    let mut changed = skill_claude("target");
    changed.pop();
    assert!(changed.len() < old.len() as usize);
    changed.push_str(&" ".repeat(old.len() as usize - changed.len() - 1));
    changed.push('\n');
    std::fs::write(&paths[0], changed).unwrap();
    let new = std::fs::metadata(&paths[0]).unwrap();
    assert_eq!(
        (old.dev(), old.ino(), old.len()),
        (new.dev(), new.ino(), new.len())
    );
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let (answer, _) =
        hydration_run_query(&store, &hydration_query_request(&graph, "target"), &owner);
    assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
    assert_eq!(answer["data"]["value"].as_bool(), Some(true));
}

#[test]
fn typed_skills_source_mutation_during_framing_is_not_a_complete_negative() {
    let source = Source::new(&format!("{}\n", user("root")));
    let paths = hydration_sources(&source, 1);
    let store = hydration_store();
    let owner = context("typed-skills-mutation");
    let root = hydration_root(&store, &source, &owner);
    let graph = hydration_graph(&store, &root, &paths, &owner);
    let child = paths[0].clone();
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::write(&child, skill_claude("target")).unwrap();
    }));
    let (answer, _) =
        hydration_run_query(&store, &hydration_query_request(&graph, "target"), &owner);
    assert_eq!(answer["status"].as_str(), Some("changed"), "{answer:?}");
    assert!(store.lock_state().prepared_queries.is_empty());
    assert_eq!(
        store.lock_state().skill_admissions.load(Ordering::Acquire),
        0
    );
}

#[test]
fn typed_skills_gateway_duplicate_keys_follow_native_classification() {
    for (header, expected) in [
        (
            r#"{"type":"session_meta","payload":{},"type":"ignored","payload":null}"#,
            true,
        ),
        (
            r#"{"type":"ignored","payload":null,"type":"session_meta","payload":{}}"#,
            false,
        ),
    ] {
        let source = Source::new(&format!("{}\n", user("root")));
        let child = source.directory.join("looks-codex.jsonl");
        std::fs::write(&child, format!("{header}\n{}", skill_claude("target"))).unwrap();
        let store = hydration_store();
        let owner = context("typed-skills-duplicate-type");
        let root = hydration_root(&store, &source, &owner);
        let graph = hydration_graph(&store, &root, &[child], &owner);
        let (answer, _) =
            hydration_run_query(&store, &hydration_query_request(&graph, "target"), &owner);
        assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
        assert_eq!(answer["data"]["value"].as_bool(), Some(expected));
    }
}

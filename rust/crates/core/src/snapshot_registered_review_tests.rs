fn registered_review_spec() -> Arc<crate::mining::CompiledMiningSpec> {
    let confidence = json!({"stages":[{"kind":"Base","band":0.9,"reason":"base"}]});
    Arc::new(
        crate::mining::compile_spec(
            &sonic_rs::to_string(&json!({
                "detectors":["transcript_message","denial","interrupt","ask_user_question"],
                "reentry_lookback":40,"edit_tools":["Edit"],"plan_tools":["ExitPlanMode"],
                "denial_excluded_tools":[],"provenance":{"subagent_tools":[]},
                "user_message":confidence,"calibrated":confidence,"review":{"surfaces":["typed"]}
            }))
            .unwrap(),
        )
        .unwrap(),
    )
}

fn registered_review_limits() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 128 * 1024 * 1024,
        max_source_read_bytes: 128 * 1024 * 1024,
        max_events: 100000,
        max_items: 10000,
        max_output_bytes: 1024 * 1024,
        max_discovery_entries: 1,
        max_sources: 1,
        deadline_unix_ms: u64::MAX,
    }
}

fn registered_review_read(
    store: &NativeStore,
    path: &Path,
    spec: Arc<crate::mining::CompiledMiningSpec>,
) -> RegisteredReviewRead<String> {
    let policy = json!({"id":"fixture-review","version":"1"});
    store
        .with_registered_claude_review(
            path,
            &context("registered-reader"),
            &policy,
            &spec,
            "reviewer-marker",
            registered_review_limits(),
            &Cancellation::default(),
            |input, _, budget| {
                assert_eq!(input.mining.accounted_bytes(), input.mining.audited_bytes());
                let rows = input.mining.signals()?.collect::<Vec<_>>();
                let value = sonic_rs::to_string(&rows).unwrap();
                budget.reserve_output(value.len())?;
                Ok(value)
            },
        )
        .unwrap()
}

fn registered_reference(source: &Source, spec: &crate::mining::CompiledMiningSpec) -> String {
    let bytes = std::fs::read(&source.path).unwrap();
    let entries = crate::parse::parse_bytes(&bytes, |_| true).unwrap();
    sonic_rs::to_string(&crate::mining::mine_entries(entries.iter().collect(), spec).unwrap())
        .unwrap()
}

#[test]
fn registered_review_crossroot_append_reads_new_bytes_and_keeps_unchanged_calls_at_zero() {
    let mut contents = format!("{}\n", user("old-user"));
    let text = "x".repeat(128 * 1024);
    for event in 0..256 {
        contents.push_str(
            &sonic_rs::to_string(&json!({
                "type":"assistant","sessionId":"s","uuid":format!("a-{event}"),
                "timestamp":"2026-11-02T00:00:00Z",
                "message":{"role":"assistant","model":"m","content":[{"type":"text","text":text}]}
            }))
            .unwrap(),
        );
        contents.push('\n');
    }
    let a = Source::new(&contents);
    let b = Source::new(&contents);
    let store = NativeStore::new(&json!({
        "max_retained_bytes":96*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_source_bytes":128*1024*1024,"max_entry_bytes":1024*1024
    }))
    .unwrap();
    let spec = registered_review_spec();
    let first = registered_review_read(&store, &a.path, Arc::clone(&spec));
    assert_eq!(first.value, registered_reference(&a, &spec));
    assert_eq!(first.progress.source_bytes, contents.len() + 64);
    for _ in 0..2 {
        let unchanged = registered_review_read(&store, &a.path, Arc::clone(&spec));
        assert_eq!(unchanged.progress.source_bytes, 0);
        assert_eq!(unchanged.progress.parsed_events, 0);
        assert_eq!(unchanged.mining_usage.events, 0);
        assert_eq!(unchanged.value, first.value);
    }
    registered_review_read(&store, &b.path, Arc::clone(&spec));
    let appended = format!("{}\n", user("new-user"));
    a.append(&appended);
    let changed = registered_review_read(&store, &a.path, Arc::clone(&spec));
    assert_eq!(changed.value, registered_reference(&a, &spec));
    assert_eq!(changed.progress.source_bytes, appended.len() + 128);
    assert_eq!(changed.progress.parsed_events, 1);
    assert_eq!(changed.mining_usage.events, 1);
    let unchanged = registered_review_read(&store, &a.path, spec);
    assert_eq!(unchanged.progress.source_bytes, 0);
    assert_eq!(unchanged.progress.parsed_events, 0);
    let mut state = store.lock_state();
    assert_eq!(state.review_inputs.len(), 2);
    assert!(NativeStore::retained_total(&mut state) < 8 * 1024 * 1024);
    for (_, accounted, walked) in NativeStore::walked_records(&state) {
        assert_eq!(accounted, walked);
    }
    println!("REGISTERED_REVIEW_CROSSROOT source={} append={} changed_read={} final_read={} accounted={}",
        contents.len(), appended.len(), changed.progress.source_bytes, unchanged.progress.source_bytes,
        NativeStore::retained_total(&mut state));
}

#[test]
fn registered_review_partial_record_completion_resumes_at_committed_bytes() {
    let complete = format!("{}\n", user("first"));
    let second = format!("{}\n", user("second"));
    let cut = second.len() / 2;
    let source = Source::new(&format!("{complete}{}", &second[..cut]));
    let store = store();
    let spec = registered_review_spec();
    let initial = registered_review_read(&store, &source.path, Arc::clone(&spec));
    assert!(!initial.complete);
    let warm = registered_review_read(&store, &source.path, Arc::clone(&spec));
    assert_eq!(warm.progress.source_bytes, 0);
    assert!(!warm.complete);
    source.append(&second[cut..]);
    let changed = registered_review_read(&store, &source.path, Arc::clone(&spec));
    assert!(changed.complete);
    assert_eq!(changed.value, registered_reference(&source, &spec));
    assert_eq!(changed.progress.parsed_events, 1);
    assert_eq!(changed.progress.source_bytes, second.len() + 128);
}

#[test]
fn registered_review_equal_size_edit_invalidates_and_failed_publication_exposes_no_partial_index() {
    let source = Source::new(&format!("{}\n", user("a")));
    let store = store();
    let spec = registered_review_spec();
    registered_review_read(&store, &source.path, Arc::clone(&spec));
    std::fs::write(&source.path, format!("{}\n", user("b"))).unwrap();
    File::options()
        .write(true)
        .open(&source.path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(1))
        .unwrap();
    let changed = registered_review_read(&store, &source.path, Arc::clone(&spec));
    assert_eq!(changed.progress.cache_invalidations, 1);
    assert_eq!(changed.value, registered_reference(&source, &spec));
    source.append(&format!("{}\n", user("c")));
    let result = store.with_registered_claude_review(
        &source.path,
        &context("registered-reader"),
        &json!({"id":"fixture-review","version":"1"}),
        &spec,
        "reviewer-marker",
        registered_review_limits(),
        &Cancellation::default(),
        |_, _, _| {
            Err::<(), _>(
                SnapshotError::new(Status::Cancelled, "injected publication refusal").into(),
            )
        },
    );
    assert!(result.is_err());
    let state = store.lock_state();
    assert_eq!(state.review_inputs.len(), 0);
    assert_eq!(state.transient_bytes, 0);
    assert!(state
        .cache_order
        .iter()
        .all(|(_, owner)| !matches!(owner, CacheOwner::Review(_))));
}

#[test]
fn registered_review_retains_no_compiled_policy_and_reuses_an_equivalent_transient_spec() {
    let source = Source::new(&format!("{}\n", user("a")));
    let store = store();
    let spec = registered_review_spec();
    let borrowed = Arc::downgrade(&spec);
    let first = registered_review_read(&store, &source.path, Arc::clone(&spec));
    assert_eq!(Arc::strong_count(&spec), 1);
    drop(spec);
    assert!(borrowed.upgrade().is_none());
    let equivalent = registered_review_spec();
    let warm = registered_review_read(&store, &source.path, equivalent);
    assert_eq!(warm.value, first.value);
    assert_eq!(warm.progress.source_bytes, 0);
    assert_eq!(warm.mining_usage.events, 0);
}

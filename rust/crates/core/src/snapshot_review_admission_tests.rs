fn small_turn_transcript(target: usize) -> Source {
    let source = Source::new(&format!("{}\n", user("anchor")));
    let output = "x".repeat(2048);
    let edit = "y".repeat(256);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.path)
        .unwrap();
    let mut size = 0;
    let mut index = 0;
    while size < target {
        let call = sonic_rs::to_string(&json!({"type":"assistant","sessionId":"s","uuid":format!("a{index}"),"timestamp":"2026-11-02T00:00:00Z",
            "message":{"model":"claude","content":[{"type":"tool_use","id":format!("t{index}"),"name":"Edit",
                "input":{"file_path":format!("/repo/f{}.py", index % 97),"old_string":edit,"new_string":format!("{edit}z")}}]}})).unwrap() + "\n";
        let result = sonic_rs::to_string(&json!({"type":"user","sessionId":"s","uuid":format!("r{index}"),"timestamp":"2026-11-02T00:00:00Z",
            "toolUseResult":{"output":output},
            "message":{"content":[{"type":"tool_result","tool_use_id":format!("t{index}"),"content":output}]}})).unwrap() + "\n";
        file.write_all(call.as_bytes()).unwrap();
        file.write_all(result.as_bytes()).unwrap();
        size += call.len() + result.len();
        index += 1;
    }
    source
}

fn review_context(claimant: &str) -> Value {
    let mut owner = context(claimant);
    owner.insert("admission", json!("review"));
    owner
}

fn review_acquire(path: &Path) -> Value {
    json!({"schema":SCHEMA,"id":"review-acquire","operation":"acquire","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},
        "deadline_unix_ms":now_ms()+600_000,"limits":{"max_read_bytes":512*1024*1024,"max_source_read_bytes":512*1024*1024,"max_events":1_000_000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
}

fn retained_total(store: &NativeStore, owner: &Value) -> u64 {
    let stats = store.test_request(
        &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(stats["status"].as_str(), Some("ok"), "{stats:?}");
    stats["data"]["gauges"]["retained_total_accounted_bytes"]
        .as_u64()
        .unwrap()
}

fn release_lease(store: &NativeStore, response: &Value, owner: &Value) {
    let released = store.test_request(
        &json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle(response)["lease_id"]}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
}

#[test]
fn review_admission_resumes_a_large_transcript_of_small_turns_under_the_default_cap() {
    let source = small_turn_transcript(64 * 1024 * 1024);
    let store = NativeStore::new_agent(&json!({})).unwrap();
    let owner = review_context("review-admission");
    let available = (store.config.retained - store.config.hook_bytes) as u64;
    let mut response = store.test_request(
        &review_acquire(&source.path),
        &owner,
        &Cancellation::default(),
    );
    let mut resumes = 0;
    while response["status"].as_str() == Some("incomplete") {
        resumes += 1;
        assert!(
            resumes < 4096,
            "cold review load must make bounded progress"
        );
        assert!(retained_total(&store, &owner) <= available);
        let cursor = response["cursor"].as_str().unwrap().to_owned();
        response = store.test_request(
            &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
            &owner,
            &Cancellation::default(),
        );
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(resumes > 1);
    let settled = retained_total(&store, &owner);
    assert!(settled <= available);
    release_lease(&store, &response, &owner);
    for _ in 0..3 {
        let warm = finish_prepared(
            &store,
            store.test_request(
                &review_acquire(&source.path),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(retained_total(&store, &owner), settled);
        release_lease(&store, &warm, &owner);
    }
    store.assert_conserved();
}

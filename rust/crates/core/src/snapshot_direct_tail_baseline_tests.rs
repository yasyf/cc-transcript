fn tail_cache_source(store: &NativeStore, source: &Source, owner: &Value) -> Value {
    let mut request = acquire(&source.path);
    request["limits"].insert("max_events", json!(4096));
    request["limits"].insert("max_read_bytes", json!(16 * 1024 * 1024));
    request["limits"].insert("max_source_read_bytes", json!(16 * 1024 * 1024));
    let response = finish(store, store.request(&request, owner, &Cancellation::default()), owner);
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    let released = store.request(
        &json!({"schema":SCHEMA,"id":"tail-cache-release","operation":"release","kind":"lease",
            "owner_epoch":store.owner_epoch,"token":handle(&response)["lease_id"]}),
        owner, &Cancellation::default(),
    );
    assert_eq!(released["data"]["released"].as_bool(), Some(true));
    assert_eq!(store.lock_state().leases.len(), 0);
    response
}

fn tail_cache_parity(actual: &Value, expected: &Value) {
    assert_eq!(actual["status"], expected["status"]);
    assert_eq!(actual.get("reason"), expected.get("reason"));
    assert_eq!(actual.get("data"), expected.get("data"));
}

#[test]
fn tail_cache_exact_generation_reuses_selection_and_reclips_output_without_a_lease() {
    let text = (0..400).map(|index| format!("{}\n", user_text(&index.to_string(), &"🙂x".repeat(300)))).collect::<String>();
    let source = Source::new(&text);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("tail-cache");
    tail_cache_source(&store, &source, &owner);
    let request = tail_request(&source.path, 256, 8*1024*1024);
    let cold = store.request(&request, &owner, &Cancellation::default());
    let warm = store.request(&request, &owner, &Cancellation::default());
    let reference = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let expected = reference.request(&request, &owner, &Cancellation::default());
    tail_cache_parity(&cold, &expected);
    tail_cache_parity(&warm, &expected);
    eprintln!("tail_cache_exact cold_bytes={} warm_bytes={} cold_lines={} warm_lines={}",
        cold["usage"]["source_bytes_read"], warm["usage"]["source_bytes_read"],
        cold["usage"]["events_parsed"], warm["usage"]["events_parsed"]);
    assert!(cold["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    assert_eq!(warm["usage"]["source_bytes_read"].as_u64(), Some(0));
    assert_eq!(warm["usage"]["events_parsed"].as_u64(), Some(0));
    assert_eq!(warm["usage"]["source_opens"].as_u64(), Some(1));
    assert_eq!(store.lock_state().leases.len(), 0);
    let mut clipped = request.clone();
    clipped["limits"].insert("max_output_bytes", json!(8*1024));
    let clipped_warm = store.request(&clipped, &owner, &Cancellation::default());
    tail_cache_parity(&clipped_warm, &reference.request(&clipped, &owner, &Cancellation::default()));
    assert_eq!(clipped_warm["usage"]["source_bytes_read"].as_u64(), Some(0));
    assert_eq!(clipped_warm["status"].as_str(), Some("incomplete"));
    let mut bounded = request.clone();
    bounded["limits"].insert("max_source_read_bytes", json!(text.len()/4));
    let different_budget = store.request(&bounded, &owner, &Cancellation::default());
    tail_cache_parity(&different_budget, &reference.request(&bounded, &owner, &Cancellation::default()));
    assert!(different_budget["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    let mut fewer = request.clone();
    fewer.insert("count", json!(2));
    let different_count = store.request(&fewer, &owner, &Cancellation::default());
    tail_cache_parity(&different_count, &reference.request(&fewer, &owner, &Cancellation::default()));
    assert!(different_count["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    assert_eq!(store.request(&request, &owner, &Cancellation::default())["usage"]["source_bytes_read"].as_u64(), Some(0));
    store.assert_conserved();
}

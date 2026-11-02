fn finish_reply_without_storage_padding(
    store: &NativeStore,
    request: &Value,
    owner: &Value,
) -> (Value, usize, u64) {
    let mut response = store.test_request(request, owner, &Cancellation::default());
    let mut resumes = 0;
    let mut bytes = response["usage"]["source_bytes_read"].as_u64().unwrap();
    while response["status"].as_str() == Some("incomplete") {
        assert!(resumes < 100, "{response:?}");
        response = store.test_request(
            &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":response["cursor"]}),
            owner,
            &Cancellation::default(),
        );
        bytes += response["usage"]["source_bytes_read"].as_u64().unwrap();
        resumes += 1;
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    store.assert_conserved();
    (response, resumes, bytes)
}

#[test]
fn agent_resumes_without_a_maximum_reply_storage_reservation() {
    for read_step in [128, 8 * 1024 * 1024] {
        let cap = 1024 * 1024;
        let store = NativeStore::new_agent(&json!({
            "max_read_bytes_per_step":read_step,
            "max_events_per_step":1,
            "max_entry_bytes":8192,
            "max_retained_bytes":cap,
            "reserved_hook_accounted_bytes":0,
            "max_leases":16,
            "reserved_hook_leases":0
        })).unwrap();
        let contents = format!("{}\n{}\n{}\n", user("first"), user("second"), user("third"));
        let source = Source::new(&contents);
        let owner = context("reply-resume");
        let (first, resumes, bytes) = finish_reply_without_storage_padding(&store, &acquire(&source.path), &owner);
        assert!(resumes > 0);
        assert_eq!(bytes, contents.len() as u64);
        assert!(store.retained_accounted_bytes() <= cap);
        for _ in 0..3 {
            let (warm, resumes, bytes) = finish_reply_without_storage_padding(&store, &acquire(&source.path), &owner);
            assert_eq!(resumes, 0);
            assert_eq!(bytes, 0);
            for key in ["events_parsed", "bytes_decoded", "prepared_cache_bytes_read", "activity_lifts"] {
                assert_eq!(warm["usage"][key].as_u64(), Some(0), "{key}: {warm:?}");
            }
            assert!(store.retained_accounted_bytes() <= cap);
        }
        let appended = format!("{}\n", user("fourth"));
        let before = store.lock_state().counters;
        source.append(&appended);
        let (last, _, bytes) = finish_reply_without_storage_padding(&store, &acquire(&source.path), &owner);
        assert_eq!(bytes, appended.len() as u64);
        assert_eq!(store.lock_state().counters[3] - before[3], 1);
        assert_ne!(handle(&first)["generation"], handle(&last)["generation"]);
        assert!(store.retained_accounted_bytes() <= cap);
        let snapshot = store.pin(handle(&last), &owner).unwrap();
        assert_eq!(snapshot.event_count, 4);
        assert_eq!(snapshot.entry(3).meta().unwrap().uuid.to_string(), "fourth");
    }
}

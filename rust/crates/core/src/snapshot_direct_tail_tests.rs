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

#[test]
fn tail_cache_standalone_and_changed_sources_keep_the_original_scanner() {
    let source = Source::new(&users(0..10));
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("tail-cache-change");
    let request = tail_request(&source.path, 3, 1024*1024);
    for _ in 0..2 {
        let response = store.request(&request, &owner, &Cancellation::default());
        assert!(response["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
        assert!(store.lock_state().generations.is_empty());
    }
    tail_cache_source(&store, &source, &owner);
    store.request(&request, &owner, &Cancellation::default());
    assert_eq!(store.request(&request, &owner, &Cancellation::default())["usage"]["source_bytes_read"].as_u64(), Some(0));
    for mutation in ["append", "rewrite", "truncate", "replace"] {
        match mutation {
            "append" => source.append(&users(10..12)),
            "rewrite" => std::fs::write(&source.path, users(20..32)).unwrap(),
            "truncate" => std::fs::write(&source.path, users(40..41)).unwrap(),
            "replace" => {
                let replacement = source.directory.join("replacement.jsonl");
                std::fs::write(&replacement, users(50..55)).unwrap();
                std::fs::rename(replacement, &source.path).unwrap();
            }
            _ => unreachable!(),
        }
        let changed = store.request(&request, &owner, &Cancellation::default());
        let reference = NativeStore::new(&json!({})).unwrap();
        tail_cache_parity(&changed, &reference.request(&request, &owner, &Cancellation::default()));
        assert!(changed["usage"]["source_bytes_read"].as_u64().unwrap() > 0, "{mutation}: {changed:?}");
        tail_cache_source(&store, &source, &owner);
        let cold = store.request(&request, &owner, &Cancellation::default());
        let warm = store.request(&request, &owner, &Cancellation::default());
        tail_cache_parity(&warm, &cold);
        assert_eq!(warm["usage"]["source_bytes_read"].as_u64(), Some(0), "{mutation}: {warm:?}");
        store.assert_conserved();
    }
}

#[test]
fn tail_cache_preserves_metadata_skipped_lines_provisional_offsets_and_current_authority() {
    let text = format!("{}\n\nnot-json\n[]\n{{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}}\n{}",
        user_text("first", "雪🙂"), user("provisional"));
    let source = Source::new(&text);
    let store = NativeStore::new(&json!({})).unwrap();
    let owner = context("tail-cache-metadata");
    tail_cache_source(&store, &source, &owner);
    let request = tail_request(&source.path, 3, 1024*1024);
    let reference = NativeStore::new(&json!({})).unwrap();
    let cold = store.request(&request, &owner, &Cancellation::default());
    let warm = store.request(&request, &owner, &Cancellation::default());
    tail_cache_parity(&warm, &reference.request(&request, &owner, &Cancellation::default()));
    tail_cache_parity(&warm, &cold);
    assert_eq!(warm["usage"]["source_bytes_read"].as_u64(), Some(0));
    let records: Vec<_> = warm["data"]["records_json"].as_array().unwrap().iter()
        .map(|record| record.as_str().unwrap().to_owned()).collect();
    let decoded = crate::snapshot_codec::decode_events(&records, 1024*1024).unwrap();
    assert_eq!(decoded.len(), 3);
    let ids: Vec<_> = decoded.iter().filter_map(|record| record.event.meta().map(|meta| meta.uuid.as_str())).collect();
    assert_eq!(ids, ["first", "provisional"]);
    let mut denied = owner.clone();
    denied.insert("authority", json!({"kind":"restricted_roots","roots":[],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    let refused = store.request(&request, &denied, &Cancellation::default());
    assert_eq!(refused["status"].as_str(), Some("permission_denied"));
    assert_eq!(refused["usage"]["source_bytes_read"].as_u64(), Some(0));
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert_eq!(store.request(&request, &owner, &cancelled)["status"].as_str(), Some("cancelled"));
    let mut expired = request.clone();
    expired.insert("deadline_unix_ms", json!(1));
    assert_eq!(store.request(&expired, &owner, &Cancellation::default())["status"].as_str(), Some("deadline"));
    store.assert_conserved();
}

#[test]
fn tail_cache_retained_selection_survives_record_registration_and_drops_with_source() {
    let source = Source::new(&users(0..10));
    let store = NativeStore::new(&json!({})).unwrap();
    let owner = context("tail-cache-owner");
    tail_cache_source(&store, &source, &owner);
    let request = tail_request(&source.path, 3, 1024*1024);
    store.request(&request, &owner, &Cancellation::default());
    let stamp = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    let cached_source = store.direct_tail_source(stamp, &owner).unwrap();
    let key = snapshot_key(&cached_source);
    let (weak, spans) = {
        let state = store.lock_state();
        let record = state.generations.get(&key).unwrap();
        let tail = record.direct_tail.as_ref().unwrap();
        assert_eq!(NativeStore::audit_direct_tail_bytes(tail), tail.charge.owned_capacity_bytes + tail.charge.opaque_dom_accounted_bytes);
        (Arc::downgrade(tail), record.source_chunks.clone())
    };
    let mut replacement = GenerationRecord::new(&cached_source, owner["registry_generation"].as_str().unwrap());
    replacement.source_chunks = spans;
    store.lock_state().register_generation(&cached_source, replacement);
    assert_eq!(store.request(&request, &owner, &Cancellation::default())["usage"]["source_bytes_read"].as_u64(), Some(0));
    store.assert_conserved();
    drop(cached_source);
    store.lock_state().evict_cache_owner(CacheOwner::Generation(key));
    store.lock_state().drain();
    assert!(weak.upgrade().is_none());
    store.assert_conserved();
}

#[test]
fn tail_cache_rechecks_retargeted_authority_before_reuse() {
    let mut source = Source::new(&users(0..8));
    let allowed = source.directory.join("allowed");
    let nested = allowed.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let path = nested.join("s.jsonl");
    std::fs::rename(&source.path, &path).unwrap();
    source.path = path;
    let store = NativeStore::new(&json!({})).unwrap();
    let mut owner = context("tail-cache-retarget");
    owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy().as_ref()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    tail_cache_source(&store, &source, &owner);
    let request = tail_request(&source.path, 3, 1024*1024);
    store.request(&request, &owner, &Cancellation::default());
    assert_eq!(store.request(&request, &owner, &Cancellation::default())["usage"]["source_bytes_read"].as_u64(), Some(0));
    let outside = source.directory.join("outside");
    std::fs::rename(&nested, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &nested).unwrap();
    let refused = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(refused["status"].as_str(), Some("permission_denied"));
    assert_eq!(refused["usage"]["source_bytes_read"].as_u64(), Some(0));
    store.assert_conserved();
}

#[test]
fn tail_cache_cancelled_cold_scan_does_not_publish_a_selection() {
    let source = Source::new(&users(0..8));
    let store = NativeStore::new(&json!({})).unwrap();
    let owner = context("tail-cache-cancel");
    tail_cache_source(&store, &source, &owner);
    let cancel = Arc::new(Cancellation::default());
    let observed = Arc::clone(&cancel);
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || observed.cancel()));
    let response = store.request(&tail_request(&source.path, 3, 1024*1024), &owner, &cancel);
    assert_eq!(response["status"].as_str(), Some("cancelled"));
    assert!(store.lock_state().generations.values().all(|record| record.direct_tail.is_none()));
    store.assert_conserved();
}

#[test]
fn tail_cache_registry_mismatch_keeps_the_original_scanner() {
    let source = Source::new(&users(0..8));
    let store = NativeStore::new(&json!({})).unwrap();
    let owner = context("tail-cache-registry");
    tail_cache_source(&store, &source, &owner);
    let request = tail_request(&source.path, 3, 1024*1024);
    let cold = store.request(&request, &owner, &Cancellation::default());
    let warm = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(warm["usage"]["source_bytes_read"].as_u64(), Some(0));
    let mut different = owner.clone();
    different.insert("registry_generation", json!(register_changed_registry(&store)));
    assert_ne!(different["registry_generation"], owner["registry_generation"]);
    for _ in 0..2 {
        let response = store.request(&request, &different, &Cancellation::default());
        tail_cache_parity(&response, &cold);
        assert!(response["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
        assert_eq!(response["usage"]["cache_hits"].as_u64(), Some(0));
    }
    assert_eq!(store.request(&request, &owner, &Cancellation::default())["usage"]["source_bytes_read"].as_u64(), Some(0));
    store.assert_conserved();
}

#[test]
fn tail_cache_optional_retention_refusal_keeps_the_original_empty_reply() {
    let source = Source::new("");
    let mut store = NativeStore::new(&json!({})).unwrap();
    let owner = context("tail-cache-refusal");
    tail_cache_source(&store, &source, &owner);
    let stamp = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    let pinned = store.direct_tail_source(stamp, &owner).unwrap();
    let key = snapshot_key(&pinned);
    let before = NativeStore::retained_total(&mut store.lock_state());
    let original_cap = store.config.retained;
    store.config.retained = before + MAX_REPLY_BYTES.saturating_mul(2);
    let request = tail_request(&source.path, 3, 1024*1024);
    let refused_retention = store.request(&request, &owner, &Cancellation::default());
    let reference = NativeStore::new(&json!({})).unwrap();
    tail_cache_parity(&refused_retention, &reference.request(&request, &owner, &Cancellation::default()));
    assert_eq!(refused_retention["status"].as_str(), Some("ok"));
    assert_eq!(refused_retention["usage"]["source_bytes_read"].as_u64(), Some(0));
    assert!(store.lock_state().generations.get(&key).unwrap().direct_tail.is_none());
    assert_eq!(NativeStore::retained_total(&mut store.lock_state()), before);
    store.assert_conserved();
    store.config.retained = original_cap;
    let admitted = store.request(&request, &owner, &Cancellation::default());
    tail_cache_parity(&admitted, &refused_retention);
    assert!(store.lock_state().generations.get(&key).unwrap().direct_tail.is_some());
    store.assert_conserved();
}

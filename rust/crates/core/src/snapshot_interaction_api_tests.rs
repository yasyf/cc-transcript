fn interaction_api_request(acquired: &Value, query: Value) -> Value {
    json!({"schema":SCHEMA,"id":"interaction-query","operation":"query",
        "view":{"handle":handle(acquired),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
        "query":query,"deadline_unix_ms":now_ms()+30_000,
        "limits":{"max_read_bytes":64*1024*1024,"max_source_read_bytes":32*1024*1024,"max_events":100_000,
            "max_items":65_536,"max_output_bytes":16*1024*1024,"max_discovery_entries":1000,"max_sources":1}})
}

#[test]
fn native_interaction_api_avoids_unrelated_bodies_and_preserves_meta_provenance() {
    let mut body = format!("{}\n", user("first"));
    body.push_str(&interaction_call("giant", "unrelated", "Bash", &format!(r#"{{"command":"{}"}}"#, "x".repeat(2*1024*1024))));
    body.push('\n');
    body.push_str(&interaction_call("question", "q", "AskUserQuestion", r#"{"questions":[]}"#));
    body.push('\n');
    body.push_str(r#"{"type":"user","uuid":"card","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","isMeta":true,"sourceToolUseID":"","sourceToolAssistantUUID":null,"message":{"content":"slack.action exact text"}}"#);
    body.push('\n');
    let source = Source::new(&body);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,"max_read_bytes_per_step":8*1024*1024,"max_events_per_step":16384,"max_entry_bytes":8*1024*1024})).unwrap();
    let owner = context("interaction-api");
    let mut request = acquire(&source.path);
    request["limits"]["max_read_bytes"] = json!(8*1024*1024);
    request["limits"]["max_source_read_bytes"] = json!(8*1024*1024);
    let acquired = finish(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let keys = store.request(&interaction_api_request(&acquired, json!({"kind":"interaction_keys","name":"AskUserQuestion"})), &owner, &Cancellation::default());
    assert_eq!(keys["status"].as_str(), Some("ok"), "{keys:?}");
    assert_eq!(crate::snapshot_codec::decode_interaction_keys(&[keys["data"]["records_json"][0].as_str().unwrap().to_owned()], 1024).unwrap()[0].key, "q");
    let winners = store.request(&interaction_api_request(&acquired, json!({"kind":"interaction_winners","keys":["q","absent"]})), &owner, &Cancellation::default());
    assert_eq!(winners["status"].as_str(), Some("ok"), "{winners:?}");
    let records = winners["data"]["records_json"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let winners = crate::snapshot_codec::decode_interaction_winners(&records, 4096).unwrap();
    assert_eq!(winners[0].name.as_deref(), Some("AskUserQuestion"));
    assert_eq!(winners[0].first, Some(1));
    assert!(!winners[1].present);
    let selected = store.request(&interaction_api_request(&acquired, json!({"kind":"interaction_records","keys":["q"]})), &owner, &Cancellation::default());
    assert_eq!(selected["status"].as_str(), Some("ok"), "{selected:?}");
    let record = selected["data"]["records_json"][0].as_str().unwrap().to_owned();
    assert!(record.len() < 1024);
    assert_eq!(crate::snapshot_codec::decode_tool_uses(&[record], 4096).unwrap()[0].r#ref.tool_use_id, "q");
    let messages = store.request(&interaction_api_request(&acquired, json!({"kind":"meta_messages","contains":"slack.action"})), &owner, &Cancellation::default());
    assert_eq!(messages["status"].as_str(), Some("ok"), "{messages:?}");
    let record = messages["data"]["records_json"][0].as_str().unwrap().to_owned();
    let messages = crate::snapshot_codec::decode_meta_messages(&[record], 4096).unwrap();
    assert_eq!(messages[0].text, "slack.action exact text");
    assert!(messages[0].is_meta);
    assert_eq!(messages[0].source_tool_use_id, None);
    assert_eq!(store.lock_state().counters[1], before);
    store.assert_conserved();
}

#[test]
fn native_interaction_meta_codec_preserves_empty_source_identifiers() {
    let record = r#"{"kind":"user","text":"card","ts":"2026-01-02T03:04:07Z","is_meta":true,"source_tool_use_id":"","source_tool_assistant_uuid":"","origin":null}"#;
    let decoded = crate::snapshot_codec::decode_meta_messages(&[record.to_owned()], 4096).unwrap();
    assert_eq!(decoded[0].source_tool_use_id.as_deref(), Some(""));
    assert_eq!(decoded[0].source_tool_assistant_uuid.as_deref(), Some(""));
}

fn interaction_views_request(acquired: &Value) -> Value {
    let mut request = interaction_api_request(acquired, Value::new());
    request.insert("operation", json!("acquire_views"));
    request.insert("handle", handle(acquired).clone());
    request.insert("excerpt", json!({"forms":["first"],"around":2,"count":20}));
    request.insert("tail_events", json!(2));
    request.as_object_mut().unwrap().remove("view");
    request.as_object_mut().unwrap().remove("query");
    request
}

#[test]
fn native_interaction_views_share_one_raw_read_and_release_independently() {
    let body = format!("{}\n{}\n{}\n", user("first"), user("second"), user("No"));
    let source = Source::new(&body);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("interaction-bundle");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let request = interaction_views_request(&acquired);
    let result = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
    let cold = store.lock_state().counters[1] - before;
    assert_eq!(cold, body.len() as u64);
    assert_eq!(result["data"]["views"].as_array().unwrap().len(), 2);
    assert_eq!(result["data"]["views"][0]["label"].as_str(), Some("excerpt"));
    assert_eq!(result["data"]["views"][1]["label"].as_str(), Some("tail"));
    let excerpt = json!({"data":{"description":result["data"]["views"][0]["description"]}});
    let tail = json!({"data":{"description":result["data"]["views"][1]["description"]}});
    assert_ne!(handle(&excerpt)["lease_id"], handle(&tail)["lease_id"]);
    release_lease(&store, handle(&excerpt), &owner);
    assert!(store.pin(handle(&tail), &owner).is_ok());
    let warmed = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(warmed["status"].as_str(), Some("ok"), "{warmed:?}");
    assert_eq!(store.lock_state().counters[1] - before, cold);
    source.append(&format!("{}\n", user("Changed")));
    let changed = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(changed["status"].as_str(), Some("ok"), "{changed:?}");
    assert_ne!(changed["data"]["views"][1]["description"]["handle"]["generation"], result["data"]["views"][1]["description"]["handle"]["generation"]);
    assert_eq!(store.pin(handle(&tail), &owner).unwrap().event_count, 2);
    store.assert_conserved();
    eprintln!("interaction_views cold_source_bytes={cold} warm_source_bytes=0 views=2");
}

#[test]
fn native_interaction_views_rollback_only_new_leases_on_capacity_failure() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = NativeStore::new(&json!({"max_leases":2,"reserved_hook_leases":0,"max_retained_bytes":64*1024*1024})).unwrap();
    let owner = context("interaction-rollback");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    assert_eq!(store.lock_state().leases.len(), 1);
    let result = store.request(&interaction_views_request(&acquired), &owner, &Cancellation::default());
    assert_eq!(result["status"].as_str(), Some("lease_limit"), "{result:?}");
    assert_eq!(store.lock_state().leases.len(), 1);
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_meta_text_join_preserves_empty_and_unicode_blocks() {
    for count in [0, 1, 32, 4096] {
        let mut blocks = vec![crate::types::ContentBlock::Text(String::new()); count];
        blocks.push(crate::types::ContentBlock::Text("雪".to_owned()));
        blocks.push(crate::types::ContentBlock::Text(String::new()));
        let expected = blocks.iter().map(|block| match block {
            crate::types::ContentBlock::Text(text) => text.as_str(),
            _ => unreachable!(),
        }).collect::<Vec<_>>().join(" ");
        let actual = crate::types::joined_text(&blocks);
        assert_eq!(actual, expected);
        assert_eq!(actual.capacity(), actual.len());
    }
}

#[test]
fn native_interaction_view_classification_keeps_the_acquired_generation_after_append() {
    let source = Source::new(&format!("{}\n{}\n", user("first"), user("No")));
    let store = NativeStore::new(&json!({"max_retained_bytes":64*1024*1024})).unwrap();
    store.register_classifier("selected-view", "1", Arc::new(|_, range| Ok(vec![true; range.len()]))).unwrap();
    let owner = context("interaction-classifier");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let selected = store.request(&interaction_views_request(&acquired), &owner, &Cancellation::default());
    assert_eq!(selected["status"].as_str(), Some("ok"), "{selected:?}");
    let tail = json!({"data":{"description":selected["data"]["views"][1]["description"]}});
    source.append(&format!("{}\n", user("later")));
    let before = store.lock_state().counters[1];
    let mut request = interaction_api_request(&tail, Value::new());
    request.insert("operation", json!("classify_view"));
    request.insert("classifier", json!({"id":"selected-view","version":"1"}));
    request.as_object_mut().unwrap().remove("query");
    let classified = finish(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(classified["status"].as_str(), Some("ok"), "{classified:?}");
    assert_eq!(classified["data"]["description"]["classifier"]["id"].as_str(), Some("selected-view"));
    assert_eq!(store.pin(handle(&classified), &owner).unwrap().event_count, 2);
    assert_eq!(store.lock_state().counters[1], before);
    assert!(store.pin(handle(&tail), &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_projection_rejects_unsupported_selectors_without_source_reads() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("interaction-selector");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let mut request = interaction_api_request(&acquired, json!({"kind":"interaction_keys","name":"AskUserQuestion"}));
    request["view"].insert("selectors", json!([{"kind":"current_turn"}]));
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("invalid_request"), "{response:?}");
    assert_eq!(store.lock_state().counters[1], before);
    store.assert_conserved();
}

#[test]
fn native_interaction_index_rechecks_the_current_canonical_authority() {
    let mut source = Source::new(&format!("{}\n", user("first")));
    let allowed = source.directory.join("allowed");
    let nested = allowed.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let path = nested.join("s.jsonl");
    std::fs::rename(&source.path, &path).unwrap();
    source.path = path;
    let store = store();
    let mut owner = context("interaction-current-route");
    owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let initial = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    let escaped = source.directory.join("escaped");
    std::fs::rename(&nested, &escaped).unwrap();
    std::os::unix::fs::symlink(&escaped, &nested).unwrap();
    assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()), initial);
    let request = interaction_api_request(&acquired, json!({"kind":"interaction_keys","name":"AskUserQuestion"}));
    let denied = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(denied["status"].as_str(), Some("permission_denied"), "{denied:?}");
    assert_eq!(store.lock_state().counters[1], before);
    assert!(store.lock_state().generations.values().all(|record| record.interactions.is_none()));
    store.assert_conserved();
}

#[test]
fn native_interaction_bundle_delivery_tracks_discard_and_output_refusal() {
    let source = Source::new(&format!("{}\n{}\n", user("first"), user("last")));
    let store = NativeStore::new(&json!({"max_retained_bytes":64*1024*1024})).unwrap();
    let owner = context("interaction-delivery");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let request = interaction_views_request(&acquired);
    let response = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert_eq!(NativeStore::response_handles(&response).unwrap().len(), 2);
    assert_eq!(store.lock_state().leases.len(), 3);
    assert!(store.discard_response(&response, &owner).unwrap());
    assert_eq!(store.lock_state().leases.len(), 1);
    assert!(!store.discard_response(&response, &owner).unwrap());
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    let mut limited = request;
    limited["limits"].insert("max_output_bytes", json!(256));
    let refused = store.request(&limited, &owner, &Cancellation::default());
    assert_eq!(refused["status"].as_str(), Some("output_limit"), "{refused:?}");
    assert_eq!(store.lock_state().leases.len(), 1);
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_bundle_rejects_public_tail_bounds_before_reading() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("interaction-tail-bound");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    for count in [0, 257] {
        let mut request = interaction_views_request(&acquired);
        request.insert("tail_events", json!(count));
        let refused = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(refused["status"].as_str(), Some("invalid_request"), "{refused:?}");
        assert_eq!(store.lock_state().leases.len(), 1);
        assert_eq!(store.lock_state().counters[1], before);
        assert!(store.lock_state().generations.values().all(|record| record.raw_cache.is_none()));
    }
    store.assert_conserved();
}

#[test]
fn native_interaction_bundle_labels_raw_boundaries_native_before_explicit_classification() {
    let source = Source::new(&format!("{}\n{}\n", user("first"), user("last")));
    let store = NativeStore::new(&json!({"max_retained_bytes":64*1024*1024})).unwrap();
    store.register_classifier("exclude-user", "1", Arc::new(|_, range| Ok(vec![false; range.len()]))).unwrap();
    let owner = context("interaction-native-label");
    let mut request = acquire(&source.path);
    request.insert("classifier", json!({"id":"exclude-user","version":"1"}));
    let acquired = finish(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(acquired["data"]["description"]["classifier"]["id"].as_str(), Some("exclude-user"));
    let bundle = store.request(&interaction_views_request(&acquired), &owner, &Cancellation::default());
    assert_eq!(bundle["status"].as_str(), Some("ok"), "{bundle:?}");
    for view in bundle["data"]["views"].as_array().unwrap().iter() {
        assert_eq!(view["description"]["classifier"], json!({"id":"native","version":"1"}));
    }
    let tail = json!({"data":{"description":bundle["data"]["views"][1]["description"]}});
    let mut classify = interaction_api_request(&tail, Value::new());
    classify.insert("operation", json!("classify_view"));
    classify.insert("classifier", json!({"id":"exclude-user","version":"1"}));
    classify.as_object_mut().unwrap().remove("query");
    let result = finish(&store, store.request(&classify, &owner, &Cancellation::default()), &owner);
    assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
    assert_eq!(result["data"]["description"]["classifier"]["id"].as_str(), Some("exclude-user"));
    assert_eq!(store.pin(handle(&result), &owner).unwrap().event_count, 2);
    store.assert_conserved();
}

#[test]
fn native_interaction_classification_rolls_back_late_cancellation() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":64,"max_retained_bytes":64*1024*1024})).unwrap();
    let owner = context("interaction-classifier-cancel");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let cancel = Arc::new(Cancellation::default());
    let callback_cancel = Arc::clone(&cancel);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    store.register_classifier("cancel-view", "1", Arc::new(move |_, range| {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        callback_cancel.cancel();
        Ok(vec![true; range.len()])
    })).unwrap();
    let mut request = interaction_api_request(&acquired, Value::new());
    request.insert("operation", json!("classify_view"));
    request.insert("classifier", json!({"id":"cancel-view","version":"1"}));
    request.as_object_mut().unwrap().remove("query");
    let result = store.request(&request, &owner, &cancel);
    assert_eq!(result["status"].as_str(), Some("cancelled"), "{result:?}");
    assert!(calls.load(Ordering::Relaxed) > 0);
    assert_eq!(store.lock_state().leases.len(), 1);
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_classification_checks_current_route_before_and_after_callback() {
    for during in [false, true] {
        let mut source = Source::new(&format!("{}\n", user("first")));
        let allowed = source.directory.join("allowed");
        let nested = allowed.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("s.jsonl");
        std::fs::rename(&source.path, &path).unwrap();
        source.path = path;
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_events_per_step":64,"max_retained_bytes":64*1024*1024})).unwrap();
        let mut owner = context("interaction-classifier-route");
        owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
        let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let initial = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
        let escaped = source.directory.join("escaped");
        let moved_from = nested.clone();
        let moved_to = escaped.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        store.register_classifier("route-view", "1", Arc::new(move |_, range| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
            if during {
                std::fs::rename(&moved_from, &moved_to).unwrap();
                std::os::unix::fs::symlink(&moved_to, &moved_from).unwrap();
            }
            Ok(vec![true; range.len()])
        })).unwrap();
        if !during {
            std::fs::rename(&nested, &escaped).unwrap();
            std::os::unix::fs::symlink(&escaped, &nested).unwrap();
        }
        let before = store.lock_state().counters[1];
        let mut request = interaction_api_request(&acquired, Value::new());
        request.insert("operation", json!("classify_view"));
        request.insert("classifier", json!({"id":"route-view","version":"1"}));
        request.as_object_mut().unwrap().remove("query");
        let result = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(result["status"].as_str(), Some("permission_denied"), "{result:?}");
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(during));
        assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()), initial);
        assert_eq!(store.lock_state().counters[1], before);
        assert_eq!(store.lock_state().leases.len(), 1);
        store.assert_conserved();
    }
}

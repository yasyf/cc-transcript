fn interaction_api_limits() -> Value {
    json!({"max_read_bytes":64*1024*1024,"max_source_read_bytes":32*1024*1024,"max_events":100_000,
        "max_items":65_536,"max_output_bytes":16*1024*1024,"max_discovery_entries":1000,"max_sources":1})
}

fn interaction_api_request(handle: &Value, query: Value) -> Value {
    json!({"schema":SCHEMA,"id":"interaction-query","operation":"query",
        "view":{"handle":handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
        "query":query,"deadline_unix_ms":now_ms()+30_000,"limits":interaction_api_limits()})
}

fn interaction_views_request(handle: &Value) -> Value {
    json!({"schema":SCHEMA,"id":"interaction-query","operation":"acquire_views","handle":handle,
        "excerpt":{"forms":["first"],"around":2,"count":20},"tail_events":2,
        "deadline_unix_ms":now_ms()+30_000,"limits":interaction_api_limits()})
}

fn interaction_classify_request(handle: &Value, classifier: Value) -> Value {
    json!({"schema":SCHEMA,"id":"interaction-query","operation":"classify_view",
        "view":{"handle":handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
        "classifier":classifier,"deadline_unix_ms":now_ms()+30_000,"limits":interaction_api_limits()})
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
    let keys = store.request(&interaction_api_request(handle(&acquired), json!({"kind":"interaction_keys","name":"AskUserQuestion"})), &owner, &Cancellation::default());
    assert_eq!(keys["status"].as_str(), Some("ok"), "{keys:?}");
    assert_eq!(crate::snapshot_codec::decode_interaction_keys(&[keys["data"]["records_json"][0].as_str().unwrap().to_owned()], 1024).unwrap()[0].key, "q");
    let winners = store.request(&interaction_api_request(handle(&acquired), json!({"kind":"interaction_winners","keys":["q","absent"]})), &owner, &Cancellation::default());
    assert_eq!(winners["status"].as_str(), Some("ok"), "{winners:?}");
    let records = winners["data"]["records_json"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let winners = crate::snapshot_codec::decode_interaction_winners(&records, 4096).unwrap();
    assert_eq!(winners[0].name.as_deref(), Some("AskUserQuestion"));
    assert_eq!(winners[0].first, Some(1));
    assert!(!winners[1].present);
    let selected = store.request(&interaction_api_request(handle(&acquired), json!({"kind":"interaction_records","keys":["q"]})), &owner, &Cancellation::default());
    assert_eq!(selected["status"].as_str(), Some("ok"), "{selected:?}");
    let record = selected["data"]["records_json"][0].as_str().unwrap().to_owned();
    assert!(record.len() < 1024);
    assert_eq!(crate::snapshot_codec::decode_tool_uses(&[record], 4096).unwrap()[0].r#ref.tool_use_id, "q");
    let messages = store.request(&interaction_api_request(handle(&acquired), json!({"kind":"meta_messages","contains":"slack.action"})), &owner, &Cancellation::default());
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

#[test]
fn native_interaction_views_share_one_raw_read_and_release_independently() {
    let body = format!("{}\n{}\n{}\n", user("first"), user("second"), user("No"));
    let source = Source::new(&body);
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("interaction-bundle");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let request = interaction_views_request(handle(&acquired));
    let result = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
    let cold = store.lock_state().counters[1] - before;
    assert_eq!(cold, body.len() as u64);
    assert_eq!(result["data"]["views"].as_array().unwrap().len(), 2);
    assert_eq!(result["data"]["views"][0]["label"].as_str(), Some("excerpt"));
    assert_eq!(result["data"]["views"][1]["label"].as_str(), Some("tail"));
    let excerpt = &result["data"]["views"][0]["description"];
    let tail = &result["data"]["views"][1]["description"];
    assert_ne!(&excerpt["handle"]["lease_id"], &tail["handle"]["lease_id"]);
    release_lease(&store, &excerpt["handle"], &owner);
    assert!(store.pin(&tail["handle"], &owner).is_ok());
    let warmed = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(warmed["status"].as_str(), Some("ok"), "{warmed:?}");
    assert_eq!(store.lock_state().counters[1] - before, cold);
    source.append(&format!("{}\n", user("Changed")));
    let changed = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(changed["status"].as_str(), Some("ok"), "{changed:?}");
    assert_ne!(changed["data"]["views"][1]["description"]["handle"]["generation"], result["data"]["views"][1]["description"]["handle"]["generation"]);
    assert_eq!(store.pin(&tail["handle"], &owner).unwrap().event_count, 2);
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
    let result = store.request(&interaction_views_request(handle(&acquired)), &owner, &Cancellation::default());
    assert_eq!(result["status"].as_str(), Some("lease_limit"), "{result:?}");
    assert_eq!(store.lock_state().leases.len(), 1);
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_meta_text_join_preserves_empty_and_unicode_blocks() {
    for count in [0, 1, 32, 4096] {
        let mut blocks = (0..count).map(|_| crate::types::ContentBlock::Text(String::new())).collect::<Vec<_>>();
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
    let selected = store.request(&interaction_views_request(handle(&acquired)), &owner, &Cancellation::default());
    assert_eq!(selected["status"].as_str(), Some("ok"), "{selected:?}");
    let tail = &selected["data"]["views"][1]["description"];
    source.append(&format!("{}\n", user("later")));
    let before = store.lock_state().counters[1];
    let request = interaction_classify_request(&tail["handle"], json!({"id":"selected-view","version":"1"}));
    let classified = finish(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
    assert_eq!(classified["status"].as_str(), Some("ok"), "{classified:?}");
    assert_eq!(classified["data"]["description"]["classifier"]["id"].as_str(), Some("selected-view"));
    assert_eq!(store.pin(handle(&classified), &owner).unwrap().event_count, 2);
    assert_eq!(store.lock_state().counters[1], before);
    assert!(store.pin(&tail["handle"], &owner).is_ok());
    store.assert_conserved();
}

#[test]
fn native_interaction_projection_rejects_unsupported_selectors_without_source_reads() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("interaction-selector");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let before = store.lock_state().counters[1];
    let mut request = interaction_api_request(handle(&acquired), json!({"kind":"interaction_keys","name":"AskUserQuestion"}));
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
    let request = interaction_api_request(handle(&acquired), json!({"kind":"interaction_keys","name":"AskUserQuestion"}));
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
    let request = interaction_views_request(handle(&acquired));
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
        let mut request = interaction_views_request(handle(&acquired));
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
    let bundle = store.request(&interaction_views_request(handle(&acquired)), &owner, &Cancellation::default());
    assert_eq!(bundle["status"].as_str(), Some("ok"), "{bundle:?}");
    for view in bundle["data"]["views"].as_array().unwrap().iter() {
        assert_eq!(view["description"]["classifier"], json!({"id":"native","version":"1"}));
    }
    let tail = &bundle["data"]["views"][1]["description"];
    let classify = interaction_classify_request(&tail["handle"], json!({"id":"exclude-user","version":"1"}));
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
    let request = interaction_classify_request(handle(&acquired), json!({"id":"cancel-view","version":"1"}));
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
        let request = interaction_classify_request(handle(&acquired), json!({"id":"route-view","version":"1"}));
        let result = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(result["status"].as_str(), Some("permission_denied"), "{result:?}");
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(during));
        assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()), initial);
        assert_eq!(store.lock_state().counters[1], before);
        assert_eq!(store.lock_state().leases.len(), 1);
        store.assert_conserved();
    }
}

fn instruction_api_records(reply: &Value) -> Vec<Value> {
    reply["data"]["records_json"].as_array().unwrap().iter()
        .map(|record| sonic_rs::from_str(record.as_str().unwrap()).unwrap()).collect()
}

fn instruction_api_oracle(snapshot: &TranscriptSnapshot, range: Range<usize>) -> Vec<Value> {
    let entries: Vec<_> = (0..snapshot.event_count).map(|position| snapshot.entry(position)).collect();
    let indices: Vec<_> = (0..snapshot.activity.turn_count()).collect();
    let lift = snapshot.activity.project(&snapshot.session_id, &entries, &indices);
    let window = crate::query::Session::from_lift(&lift).windowed(range.start, range.end);
    window.turn_views().map(|(index, prompt, events, uses)| {
        let queued: Vec<_> = events.iter().filter_map(|event| {
            let crate::types::Entry::Attachment(attachment) = event else { return None; };
            let crate::types::AttachmentDetail::QueuedCommand(command) = &attachment.detail else { return None; };
            (command.origin.as_deref() == Some("human")).then(|| json!({"text":command.prompt.as_deref().unwrap_or(""),"ts":attachment.meta.timestamp}))
        }).collect();
        let rejections: Vec<_> = uses.iter().filter_map(|use_| {
            let result = use_.result?;
            (result.denial_kind.as_deref() == Some(crate::protocol::DENIAL_KIND_USER_REJECTED))
                .then(|| json!({"name":use_.call.name(),"content":result.content,"result_ts":use_.result_ts.unwrap_or(use_.ts)}))
        }).collect();
        let assistant = events.iter().filter_map(|event| {
            let Entry::Assistant(assistant) = event else { return None; };
            let text = crate::types::joined_text(&assistant.blocks);
            let text = crate::pystr::strip(&text);
            (!text.is_empty()).then(|| text.to_owned())
        }).last().unwrap_or_default();
        let assistant_tail: String = assistant.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        json!({"index":index,"prompt":prompt,"started_at":lift.turns[index].started_at,"queued":queued,
            "rejections":rejections,"assistant_tail":assistant_tail})
    }).collect()
}

fn instruction_api_body() -> String {
    let mut body = format!("{}\n", user("first"));
    body.push_str(&interaction_call("call", "rejected", "Bash", r#"{"command":"secret command"}"#));
    body.push('\n');
    body.push_str("{\"type\":\"attachment\",\"uuid\":\"queued\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:06+05:30\",\"attachment\":{\"type\":\"queued_command\",\"origin\":{\"kind\":\"human\"},\"prompt\":\"queued words\"}}\n");
    body.push_str("{\"type\":\"assistant\",\"uuid\":\"answer\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:06Z\",\"message\":{\"model\":\"test\",\"content\":[{\"type\":\"text\",\"text\":\" offer 🦀 \"}]}}\n");
    body.push_str(&format!("{}\n", user("second")));
    body.push_str(&format!("{}\n", json!({"type":"user","uuid":"result","sessionId":"s","timestamp":"2026-01-02T03:04:07-04:00","toolDenialKind":crate::protocol::DENIAL_KIND_USER_REJECTED,"message":{"content":[{"type":"tool_result","tool_use_id":"rejected","content":"No, don't do that"}]}})));
    body.push_str("{\"type\":\"assistant\",\"uuid\":\"last\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:08Z\",\"message\":{\"model\":\"test\",\"content\":[{\"type\":\"text\",\"text\":\"new offer\"}]}}\n");
    body
}

#[test]
fn native_instruction_metadata_selectors_match_full_turn_oracle() {
    let source = Source::new(&instruction_api_body());
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("instruction-oracle");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let snapshot = store.pin(handle(&acquired), &owner).unwrap();
    assert_eq!(snapshot.event_count, 7);
    for (selector, range) in [
        (None, 0..7),
        (Some(json!({"kind":"current_turn"})), 4..7),
        (Some(json!({"kind":"recent_events","count":3})), 4..7),
        (Some(json!({"kind":"recent_messages","count":4})), 3..7),
        (Some(json!({"kind":"prior"})), 0..6),
        (Some(json!({"kind":"event_range","start":1,"stop":4})), 1..4),
    ] {
        for reverse in [false, true] {
            let mut request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":if reverse {"reverse"} else {"forward"}}));
            if let Some(selector) = &selector { request["view"]["selectors"] = json!([selector]); }
            let reply = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            let actual = instruction_api_records(&reply);
            let mut expected = instruction_api_oracle(&snapshot, range.clone());
            if reverse { expected.reverse(); }
            assert_eq!(actual, expected, "selector={selector:?}");
            let records = reply["data"]["records_json"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect::<Vec<_>>();
            assert_eq!(crate::snapshot_codec::decode_instruction_turns(&records, 65536).unwrap().len(), actual.len());
        }
    }
    let mut request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":"forward"}));
    request["view"]["selectors"] = json!([{"kind":"after_last_tool","name":"Bash","file":null}]);
    let reply = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(reply["status"].as_str(), Some("invalid_request"), "{reply:?}");
    assert!(reply["data"].is_null());
    store.assert_conserved();
}

#[test]
fn native_instruction_small_window_cost_ignores_historical_success_payloads() {
    let mut baseline = None;
    for count in [32_usize, 4096] {
        let mut body = format!("{}\n", user("first"));
        for index in 0..count {
            body.push_str(&interaction_call(&format!("c{index}"), &format!("k{index}"), "Bash", &format!(r#"{{"command":"{}"}}"#, "x".repeat(if index + 1 == count { 2*1024*1024 } else { 512 }))));
            body.push('\n');
            body.push_str(&format!("{}\n", tool_result_event(&format!("k{index}"), &"y".repeat(512))));
        }
        body.push_str("{\"type\":\"assistant\",\"uuid\":\"answer\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:08Z\",\"message\":{\"model\":\"test\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n");
        let source = Source::new(&body);
        let store = NativeStore::new(&json!({"max_retained_bytes":256*1024*1024,"max_read_bytes_per_step":16*1024*1024,"max_events_per_step":16384,"max_entry_bytes":8*1024*1024})).unwrap();
        let owner = context("instruction-cost");
        let mut acquiring = acquire(&source.path);
        acquiring["limits"] = interaction_api_limits();
        let acquired = finish(&store, store.request(&acquiring, &owner, &Cancellation::default()), &owner);
        let snapshot = store.pin(handle(&acquired), &owner).unwrap();
        let mut request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":"forward"}));
        request["view"]["selectors"] = json!([{"kind":"recent_events","count":8}]);
        crate::snapshot_activity::reset_instruction_call_work();
        let before = store.lock_state().counters[1];
        let reply = store.request(&request, &owner, &Cancellation::default());
        let (comparisons, visits) = crate::snapshot_activity::instruction_call_work();
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(visits, 9);
        assert!(comparisons <= 6*(count.ilog2() as usize+2));
        let actual = instruction_api_records(&reply);
        let actual_bytes = sonic_rs::to_vec(&actual).unwrap().len();
        assert!(actual_bytes < 1024);
        assert_eq!(actual, instruction_api_oracle(&snapshot, snapshot.event_count-8..snapshot.event_count));
        assert_eq!(store.lock_state().counters[1], before);
        assert_eq!(reply["usage"]["source_bytes_read"].as_u64(), Some(0));
        let mut reservation = store.reserve_projection(&owner, 0).unwrap();
        let page = store.instruction_projection(&snapshot, &request, &owner, &Cancellation::default(), limits(&request).unwrap(), 0, &mut reservation).unwrap();
        assert!(page.read_bytes > 0 && page.read_bytes < 65536);
        assert!(page.events > 0 && page.events < 256);
        if let Some(expected) = baseline { assert_eq!((actual_bytes, page.read_bytes, page.events), expected); }
        baseline = Some((actual_bytes, page.read_bytes, page.events));
        drop(page);
        drop(reservation);
        assert!(reply["data"]["records_json"][0].as_str().unwrap().contains("done"));
        eprintln!("instruction_public historical_calls={count} comparisons={comparisons} visits={visits} output_bytes={actual_bytes} reply={reply:?}");
        store.assert_conserved();
    }
}

#[test]
fn native_instruction_refuses_budget_and_cancellation_without_partial_records() {
    let source = Source::new(&instruction_api_body());
    let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,"max_items_per_page":1})).unwrap();
    let owner = context("instruction-bounds");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":"forward"}));
    for (field, reason) in [("max_events","event_limit"),("max_read_bytes","read_limit")] {
        let mut bounded = request.clone();
        bounded["limits"][field] = json!(1);
        let reply = store.request(&bounded, &owner, &Cancellation::default());
        assert_eq!(reply["status"].as_str(), Some("incomplete"), "{reply:?}");
        assert_eq!(reply["reason"].as_str(), Some(reason));
        assert!(reply["data"].is_null());
        assert!(reply["cursor"].is_null());
        assert!(store.lock_state().projections.is_empty());
        store.assert_conserved();
    }
    for stage in ["instruction", "instruction_publish", "instruction_published"] {
        let cancel = Arc::new(Cancellation::default());
        let callback_cancel = Arc::clone(&cancel);
        let visits = Arc::new(AtomicUsize::new(0));
        let callback_visits = Arc::clone(&visits);
        *store.decode_hook.lock().unwrap() = Some(Arc::new(move |actual| {
            if actual == stage && callback_visits.fetch_add(1, Ordering::Relaxed) == if stage == "instruction" { 3 } else { 0 } {
                callback_cancel.cancel();
            }
        }));
        let reply = store.request(&request, &owner, &cancel);
        *store.decode_hook.lock().unwrap() = None;
        assert_eq!(reply["status"].as_str(), Some("cancelled"), "{reply:?}");
        assert!(reply["data"].is_null());
        assert!(reply["cursor"].is_null());
        assert!(visits.load(Ordering::Relaxed) > 0);
        assert!(store.lock_state().projections.is_empty());
        assert_eq!(store.lock_state().leases.len(), 1);
        store.assert_conserved();
    }
    let first = store.request(&request, &owner, &Cancellation::default());
    assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
    assert_eq!(instruction_api_records(&first)[0]["index"].as_u64(), Some(0));
    let next = store.request(&json!({"schema":SCHEMA,"id":"instruction-resume","operation":"resume","cursor":first["cursor"]}), &owner, &Cancellation::default());
    assert_eq!(next["status"].as_str(), Some("ok"), "{next:?}");
    assert_eq!(instruction_api_records(&next)[0]["index"].as_u64(), Some(1));
    assert!(next["cursor"].is_null());
    store.assert_conserved();
}

#[test]
fn native_instruction_projection_checks_current_route_at_each_publication_boundary() {
    for stage in ["before", "instruction_publish", "instruction_published"] {
        let mut source = Source::new(&instruction_api_body());
        let allowed = source.directory.join("allowed");
        let nested = allowed.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("s.jsonl");
        std::fs::rename(&source.path, &path).unwrap();
        source.path = path;
        let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,"max_items_per_page":1})).unwrap();
        let mut owner = context("instruction-authority");
        owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
        let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let escaped = source.directory.join("escaped");
        if stage == "before" {
            std::fs::rename(&nested, &escaped).unwrap();
            std::os::unix::fs::symlink(&escaped, &nested).unwrap();
        } else {
            let from = nested.clone();
            let to = escaped.clone();
            *store.decode_hook.lock().unwrap() = Some(Arc::new(move |actual| {
                if actual == stage {
                    std::fs::rename(&from, &to).unwrap();
                    std::os::unix::fs::symlink(&to, &from).unwrap();
                }
            }));
        }
        let request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":"forward"}));
        let reply = store.request(&request, &owner, &Cancellation::default());
        *store.decode_hook.lock().unwrap() = None;
        assert_eq!(reply["status"].as_str(), Some("permission_denied"), "{reply:?}");
        assert!(reply["data"].is_null());
        assert!(reply["cursor"].is_null());
        assert!(store.lock_state().projections.is_empty());
        assert_eq!(store.lock_state().leases.len(), 1);
        store.assert_conserved();
    }
}

#[test]
fn native_instruction_projection_reservation_refusal_releases_all_transient_memory() {
    let source = Source::new(&instruction_api_body());
    let mut store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024})).unwrap();
    let owner = context("instruction-memory");
    let acquired = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let snapshot = store.pin(handle(&acquired), &owner).unwrap();
    let request = interaction_api_request(handle(&acquired), json!({"kind":"instruction_turns","order":"forward"}));
    {
        let mut state = store.lock_state();
        while let Some(owner) = state.next_cache_victim() { state.evict_cache_owner(owner); }
        state.drain();
        state.drain();
    }
    store.assert_conserved();
    assert!(store.pin(handle(&acquired), &owner).is_ok());
    let retained = NativeStore::retained_total(&mut store.lock_state());
    store.config.retained = retained;
    let mut reservation = store.reserve_projection(&owner, 0).unwrap();
    let result = store.instruction_projection(&snapshot, &request, &owner, &Cancellation::default(), limits(&request).unwrap(), 0, &mut reservation);
    let error = result.err().expect("instruction allocation must be admitted");
    assert_eq!(error.status, Status::RetainedLimit);
    drop(reservation);
    assert_eq!(store.lock_state().transient_bytes, 0);
    assert!(store.lock_state().projections.is_empty());
    assert_eq!(store.lock_state().leases.len(), 1);
    store.assert_conserved();
}

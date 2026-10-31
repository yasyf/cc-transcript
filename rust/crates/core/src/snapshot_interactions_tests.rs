fn interaction_call(event: &str, id: &str, name: &str, input: &str) -> String {
    format!(r#"{{"type":"assistant","uuid":"{event}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#)
}

#[test]
fn native_interaction_index_reuses_metadata_and_clones_only_the_selected_call() {
    for count in [32, 4096] {
        let mut body = format!("{}\n", user("first"));
        for event in 0..count {
            body.push_str(&interaction_call(&format!("event-{event}"), &format!("b{event}"), "Bash", &format!(r#"{{"command":"{}"}}"#, "x".repeat(512))));
            body.push('\n');
        }
        for (event, id, name) in [("question", "q", "AskUserQuestion"), ("prior", "same", "AskUserQuestion"), ("replacement", "same", "Bash")] {
            body.push_str(&interaction_call(event, id, name, r#"{"command":"true","questions":[]}"#));
            body.push('\n');
        }
        let source = Source::new(&body);
        let store = NativeStore::new(&json!({"max_retained_bytes":128*1024*1024,"max_read_bytes_per_step":8*1024*1024,"max_events_per_step":16384})).unwrap();
        let owner = context("interaction-index");
        let mut request = acquire(&source.path);
        request["limits"]["max_read_bytes"] = json!(8*1024*1024);
        request["limits"]["max_source_read_bytes"] = json!(8*1024*1024);
        request["limits"]["max_events"] = json!(20000);
        let response = finish(&store, store.request(&request, &owner, &Cancellation::default()), &owner);
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        INTERACTION_INDEX_VISITS.with(|visits| visits.set(0));
        let scope = store.indexed_interactions(Arc::clone(&snapshot), &owner, &Cancellation::default(), now_ms()+30_000).unwrap();
        assert_eq!(INTERACTION_INDEX_VISITS.with(|visits| visits.get()), 2*(count+3));
        assert_eq!(scope.keys("AskUserQuestion").collect::<HashSet<_>>(), HashSet::from(["q", "same"]));
        let mut selected_reservation = store.reserve_projection(&owner, 0).unwrap();
        let winner = scope.winner("same").unwrap();
        assert_eq!(winner.first, count+1);
        assert_eq!(scope.call(winner, &mut selected_reservation, &owner).unwrap().name, "Bash");
        let selected = scope.call(scope.winner("q").unwrap(), &mut selected_reservation, &owner).unwrap();
        let all = snapshot.activity.project_turn_with(0, |event| snapshot.entry(event)).unwrap();
        let expected = all.tool_uses.iter().find(|call| call.tool_use_id == "q").unwrap();
        let selected_wire = crate::snapshot_codec::encode(&crate::snapshot_codec::ToolUseWire::new(&selected, "s"), 64*1024).unwrap();
        let expected_wire = crate::snapshot_codec::encode(&crate::snapshot_codec::ToolUseWire::new(expected, "s"), 64*1024).unwrap();
        assert_eq!(selected_wire, expected_wire);
        assert!(selected_wire.len() < 1024);
        let winner = scope.winner("q").unwrap();
        assert!(snapshot.activity.projected_call_at_bytes(winner.event, winner.ordinal).unwrap() < 4096);
        INTERACTION_INDEX_VISITS.with(|visits| visits.set(0));
        let warm = store.indexed_interactions(Arc::clone(&snapshot), &owner, &Cancellation::default(), now_ms()+30_000).unwrap();
        assert_eq!(INTERACTION_INDEX_VISITS.with(|visits| visits.get()), 0);
        assert!(Arc::ptr_eq(&scope.index, &warm.index));
        store.assert_conserved();
        eprintln!("interaction_index calls={} cold_metadata_visits={} warm_metadata_visits=0 selected_wire_bytes={}", count+3, 2*(count+3), selected_wire.len());
    }
}

#[test]
fn native_interaction_key_matches_python_datetime_and_id_collision_contract() {
    for (timestamp, expected) in [
        ("2026-01-02T03:04:05Z", "2026-01-02 03:04:05+00:00"),
        ("2026-01-02T03:04:05.000001999-07:30", "2026-01-02 03:04:05.000001-07:30"),
        ("2026-01-02T03:04:05.120000+05:45", "2026-01-02 03:04:05.120000+05:45"),
    ] {
        let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
        assert_eq!(interaction_key("", timestamp), expected);
        assert_eq!(interaction_key(expected, timestamp), interaction_key("", timestamp));
        assert_eq!(interaction_key("ordinary-id", timestamp), "ordinary-id");
    }
}

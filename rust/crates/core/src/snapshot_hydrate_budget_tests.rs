fn assert_hydrate_limit_propagation(reason: &str, prior_output: bool) {
    let assistant = |id: &str| {
        format!(
            r#"{{"type":"assistant","uuid":"{id}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
    };
    let source = Source::new(&format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n",
        user("a"),
        assistant("aa"),
        user("b"),
        assistant("bb"),
        user("c"),
        assistant("cc")
    ));
    let store = store();
    let owner = context("hydrate-budget");
    let cancel = Cancellation::default();
    let template = acquire(&source.path);
    let generous = limits(&template).unwrap();
    let prepared = finish(&store, store.request(&template, &owner, &cancel), &owner);
    let snapshot = store.pin(handle(&prepared), &owner).unwrap();
    let capture = |id: &str, before: usize| {
        let request = json!({"operation":"capture","view":{"attachments":[],"selectors":[]},
            "anchors":[{"session_id":"s","event_uuid":id,"tool_use_id":null}],
            "before":before,"after":0,"preview_chars":16});
        let result = crate::snapshot_projection::project(&snapshot, &request, &generous, &cancel, 0)
            .unwrap();
        assert!(result.complete, "{:?}", result.reason);
        result.data["windows_json"][0].clone()
    };
    let first = capture("a", 0);
    let second = capture("c", 1);
    let render = json!({"before":{"turn_chars":16,"tool_chars":16},
        "trigger":{"turn_chars":16,"tool_chars":16},"after":{"turn_chars":16,"tool_chars":16}});
    let single = |window: &Value| {
        json!({"operation":"hydrate","windows_json":[window],"render":render})
    };
    let first_full =
        crate::snapshot_projection::project(&snapshot, &single(&first), &generous, &cancel, 0)
            .unwrap();
    let second_full =
        crate::snapshot_projection::project(&snapshot, &single(&second), &generous, &cancel, 0)
            .unwrap();
    assert!(first_full.complete && second_full.complete);
    assert_eq!(first_full.events, 2);
    assert_eq!(second_full.events, 4);
    let mut partial_limits = generous;
    match reason {
        "read_limit" => partial_limits.max_read_bytes = second_full.read_bytes - 1,
        "event_limit" => partial_limits.max_events = first_full.events,
        _ => panic!("unexpected test limit"),
    }
    let partial = crate::snapshot_projection::project(
        &snapshot,
        &single(&second),
        &partial_limits,
        &cancel,
        0,
    )
    .unwrap();
    assert!(!partial.complete);
    assert_eq!(partial.reason.as_deref(), Some(reason));
    assert_eq!(
        partial.events,
        if reason == "read_limit" {
            second_full.events - 1
        } else {
            first_full.events
        }
    );
    assert!(partial.read_bytes > 0);
    let mut outer_limits = partial_limits;
    if prior_output {
        match reason {
            "read_limit" => outer_limits.max_read_bytes += first_full.read_bytes,
            "event_limit" => outer_limits.max_events += first_full.events,
            _ => unreachable!(),
        }
    }
    let windows = if prior_output {
        vec![first.clone(), second.clone()]
    } else {
        vec![second.clone()]
    };
    let request = json!({"schema":SCHEMA,"id":"hydrate-budget","operation":"hydrate",
        "deadline_unix_ms":outer_limits.deadline_unix_ms,"limits":outer_limits.to_json(),
        "handles":[{"session_id":"s","handle":handle(&prepared)}],
        "windows_json":windows,"render":render});
    let response = store.request(&request, &owner, &cancel);
    assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
    assert_eq!(
        response["data"]["windows"].as_array().unwrap().len(),
        usize::from(prior_output)
    );
    assert_eq!(response["usage"]["source_bytes_read"].as_u64(), Some(0));
    assert_eq!(response["usage"]["prepared_cache_bytes_read"].as_u64(), Some(0));
    if prior_output && reason == "read_limit" {
        let token = response["cursor"].as_str().expect("remaining read budget");
        {
            let state = store.state.lock().unwrap();
            let cursor = state.projections.get(token).unwrap();
            assert_eq!(cursor.next, 1);
            assert_eq!(
                cursor.limits.max_read_bytes,
                outer_limits.max_read_bytes - first_full.read_bytes - partial.read_bytes
            );
            assert_eq!(
                cursor.limits.max_events,
                outer_limits.max_events - first_full.events - partial.events
            );
        }
        let released = store.request(
            &json!({"schema":SCHEMA,"id":"release-hydrate","operation":"release","kind":"cursor","token":token}),
            &owner,
            &cancel,
        );
        assert_eq!(released["status"].as_str(), Some("ok"));
    } else {
        assert!(response["cursor"].is_null(), "{response:?}");
    }
    assert_eq!(response["reason"].as_str(), Some(reason));
    assert!(store.state.lock().unwrap().projections.is_empty());
    let positive = store.request(
        &json!({"schema":SCHEMA,"id":"hydrate-complete","operation":"hydrate",
            "deadline_unix_ms":generous.deadline_unix_ms,"limits":generous.to_json(),
            "handles":[{"session_id":"s","handle":handle(&prepared)}],
            "windows_json":[first,second],"render":render}),
        &owner,
        &cancel,
    );
    assert_eq!(positive["status"].as_str(), Some("ok"), "{positive:?}");
    assert!(positive["cursor"].is_null());
    assert_eq!(positive["data"]["windows"].as_array().unwrap().len(), 2);
    assert!(positive["data"]["windows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|window| window["availability"].as_str() == Some("full")
            && !window["rendered"].as_str().unwrap().is_empty()));
}

#[test]
fn hydrate_preserves_first_window_limit_reason() {
    for reason in ["read_limit", "event_limit"] {
        assert_hydrate_limit_propagation(reason, false);
    }
}

#[test]
fn hydrate_charges_partial_read_before_retaining_cursor() {
    assert_hydrate_limit_propagation("read_limit", true);
}

#[test]
fn hydrate_charges_partial_events_before_retaining_cursor() {
    assert_hydrate_limit_propagation("event_limit", true);
}

fn warm923_validation_fixture(count: usize) -> (Scenario, Arc<NativeStore>, Value, Value) {
    let scenario = Scenario::new(count, |index| session_line(&format!("thread-{index:04}")));
    std::fs::write(&scenario.root.path, tool_line("root", "Bash")).unwrap();
    let store = Arc::new(prepared_store());
    let owner = context_for("graph-validation", false);
    let (root, _) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    (scenario, store, owner, graph)
}

#[test]
fn cache_warm923_validation_cancellation_stops_before_the_next_member() {
    let (_scenario, store, owner, graph) = warm923_validation_fixture(923);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let cancellation = Cancellation::default();
    let cancelled = cancellation.clone();
    *store.graph_validation_hook.lock().unwrap() = Some(Arc::new(move |_| {
        if observed.fetch_add(1, Ordering::Relaxed) == 0 {
            cancelled.cancel();
        }
    }));
    let response = store.request(
        &graph_query(&graph, marker_tool(), json!([])),
        &owner,
        &cancellation,
    );
    assert_eq!(
        response["status"].as_str(),
        Some("cancelled"),
        "{response:?}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "WARM923_VALIDATION_KEPT_SCANNING_AFTER_CANCEL"
    );
    let state = store.lock_state();
    let graph = state.prepared_graphs[graph["graph_id"].as_str().unwrap()]
        .lock()
        .unwrap();
    assert!(
        !graph.validated,
        "WARM923_CANCELLED_VALIDATION_WAS_PUBLISHED"
    );
    assert!(state.retained_graph_input_owners.is_empty());
    drop(graph);
    drop(state);
    assert_eq!(ledger(&store), audited(&store));
}

#[test]
fn cache_warm923_member_metadata_does_not_hold_foreground_store_admission() {
    let (scenario, store, owner, graph) = warm923_validation_fixture(3);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (released_tx, released_rx) = mpsc::channel::<()>();
    let released_rx = Mutex::new(released_rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    *store.graph_validation_hook.lock().unwrap() = Some(Arc::new(move |_| {
        if observed.fetch_add(1, Ordering::Relaxed) == 0 {
            entered_tx.send(()).unwrap();
            let released = released_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            assert!(
                matches!(released, Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected)),
                "metadata gate was not released"
            );
        }
    }));
    let registered_graph =
        Arc::downgrade(&store.lock_state().prepared_graphs[graph["graph_id"].as_str().unwrap()]);
    let query_store = Arc::clone(&store);
    let query_owner = owner.clone();
    let query = graph_query(&graph, marker_tool(), json!([]));
    let query_thread = std::thread::spawn(move || {
        query_store.request(&query, &query_owner, &Cancellation::default())
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(5));
    let graph_lock_available = if entered.is_ok() {
        let graph = registered_graph.upgrade().unwrap();
        let available = graph.try_lock().is_ok();
        Some(available)
    } else {
        None
    };
    let (warm_thread, foreground_thread, progress_receiver, foreground_receiver) =
        if entered.is_ok() {
            let warm_store = Arc::clone(&store);
            let mut warm_owner = owner.clone();
            warm_owner.insert("work_class", json!("background"));
            let request = warm_request(&scenario);
            let (progress_tx, progress_rx) = mpsc::channel();
            let warm_thread = std::thread::spawn(move || {
                let response = warm_store.request(&request, &warm_owner, &Cancellation::default());
                progress_tx.send(response.clone()).unwrap();
                response
            });
            let foreground_store = Arc::clone(&store);
            let foreground_owner = owner.clone();
            let (foreground_tx, foreground_rx) = mpsc::channel();
            let foreground_thread = std::thread::spawn(move || {
                let response = foreground_store.request(
                    &json!({"schema":SCHEMA,"id":"foreground-stats","operation":"stats"}),
                    &foreground_owner,
                    &Cancellation::default(),
                );
                foreground_tx.send(response.clone()).unwrap();
                response
            });
            (
                Some(warm_thread),
                Some(foreground_thread),
                Some(progress_rx),
                Some(foreground_rx),
            )
        } else {
            (None, None, None, None)
        };
    let until = Instant::now() + Duration::from_secs(5);
    let progress = progress_receiver
        .as_ref()
        .map(|receiver| receiver.recv_timeout(until.saturating_duration_since(Instant::now())));
    let foreground_progress = foreground_receiver
        .as_ref()
        .map(|receiver| receiver.recv_timeout(until.saturating_duration_since(Instant::now())));
    drop(released_tx);
    let query_result = query_thread.join();
    let warm_result = warm_thread.map(|thread| thread.join());
    let foreground_result = foreground_thread.map(|thread| thread.join());
    assert!(entered.is_ok(), "metadata gate was not reached");
    let query_result = query_result.unwrap();
    let warm_result = warm_result.unwrap().unwrap();
    let foreground_result = foreground_result.unwrap().unwrap();
    assert_eq!(
        query_result["status"].as_str(),
        Some("ok"),
        "{query_result:?}"
    );
    assert_eq!(query_result["data"]["value"].as_bool(), Some(true));
    assert_eq!(
        warm_result["status"].as_str(),
        Some("ok"),
        "{warm_result:?}"
    );
    assert!(warm_result["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
    assert_eq!(
        foreground_result["status"].as_str(),
        Some("ok"),
        "{foreground_result:?}"
    );
    assert_eq!(
        graph_lock_available,
        Some(true),
        "WARM923_MEMBER_METADATA_HELD_GRAPH_LOCK"
    );
    assert!(
        foreground_progress.unwrap().is_ok(),
        "WARM923_FOREGROUND_BLOCKED_BY_MEMBER_METADATA"
    );
    assert!(
        progress.unwrap().is_ok(),
        "WARM923_BACKGROUND_BLOCKED_BY_MEMBER_METADATA"
    );
    assert_eq!(ledger(&store), audited(&store));
}

#[test]
fn cache_warm923_validation_sources_remain_charged_after_graph_removal() {
    let (_scenario, store, _owner, graph) = warm923_validation_fixture(3);
    let id = graph["graph_id"].as_str().unwrap().to_owned();
    let (weak_graph, weak_sources) = {
        let state = store.lock_state();
        let shared = &state.prepared_graphs[&id];
        let graph = shared.lock().unwrap();
        (Arc::downgrade(shared), Arc::downgrade(&graph.sources))
    };
    let before = ledger(&store);
    let weak_header = Arc::downgrade(
        &store.lock_state().prepared_graphs[&id]
            .lock()
            .unwrap()
            .header,
    );
    let retained_before = ledger(&store);
    traced(&store);
    let validation = store.borrow_prepared_graph_validation(&id).unwrap();
    assert!(traced(&store).is_empty());
    assert_eq!(ledger(&store), retained_before);
    let second = store.borrow_prepared_graph_validation(&id).unwrap();
    assert!(traced(&store).is_empty());
    assert_eq!(ledger(&store), retained_before);
    assert_eq!(store.lock_state().retained_graph_input_owners.len(), 2);
    let removed = store.lock_state().remove_prepared_graph(&id).unwrap();
    drop(removed);
    assert!(
        weak_graph.upgrade().is_none(),
        "validation retained the whole graph"
    );
    assert!(weak_sources.upgrade().is_some());
    assert_eq!(ledger(&store), audited(&store));
    assert!(weak_header.upgrade().is_some());
    drop(validation);
    assert!(weak_sources.upgrade().is_some());
    assert!(weak_header.upgrade().is_some());
    assert_eq!(store.lock_state().retained_graph_input_owners.len(), 1);
    assert_eq!(ledger(&store), audited(&store));
    drop(second);
    assert!(weak_sources.upgrade().is_none());
    assert!(weak_header.upgrade().is_none());
    assert!(store.lock_state().retained_graph_input_owners.is_empty());
    assert_eq!(ledger(&store), audited(&store));
    assert!(ledger(&store)[TOTAL] < before[TOTAL]);
}

#[test]
fn cache_warm923_removed_graph_cannot_publish_validated_evidence() {
    let (_scenario, store, owner, graph) = warm923_validation_fixture(3);
    let id = graph["graph_id"].as_str().unwrap().to_owned();
    let weak_graph = Arc::downgrade(&store.lock_state().prepared_graphs[&id]);
    let weak_store = Arc::downgrade(&store);
    let removed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&removed);
    *store.graph_validation_hook.lock().unwrap() = Some(Arc::new(move |_| {
        if !observed.swap(true, Ordering::Relaxed) {
            let store = weak_store.upgrade().unwrap();
            let graph = store.lock_state().remove_prepared_graph(&id).unwrap();
            drop(graph);
            assert!(
                weak_graph.upgrade().is_none(),
                "unlocked validation retained graph payload"
            );
            assert_eq!(store.lock_state().retained_graph_input_owners.len(), 1);
            assert_eq!(ledger(&store), audited(&store));
        }
    }));
    let response = store.request(
        &graph_query(&graph, marker_tool(), json!([])),
        &owner,
        &Cancellation::default(),
    );
    assert_eq!(
        response["status"].as_str(),
        Some("stale_handle"),
        "{response:?}"
    );
    assert!(removed.load(Ordering::Relaxed));
    assert!(store.lock_state().retained_graph_input_owners.is_empty());
    assert_eq!(ledger(&store), audited(&store));
}

#[test]
fn cache_warm923_root_positive_still_checks_the_last_member() {
    let (scenario, store, owner, graph) = warm923_validation_fixture(923);
    std::fs::write(
        scenario.sidechains.last().unwrap(),
        session_line("rewritten-member"),
    )
    .unwrap();
    let response = store.request(
        &graph_query(&graph, marker_tool(), json!([])),
        &owner,
        &Cancellation::default(),
    );
    assert_eq!(response["status"].as_str(), Some("changed"), "{response:?}");
    assert!(response["data"].is_null());
    assert!(store.lock_state().retained_graph_input_owners.is_empty());
    assert_eq!(ledger(&store), audited(&store));
}

#[test]
fn cache_warm923_final_publication_rechecks_cancellation_context_revision_and_expiry() {
    for fault in ["cancel", "context", "revision", "expiry", "root", "append"] {
        let (scenario, store, owner, graph) = warm923_validation_fixture(3);
        let id = graph["graph_id"].as_str().unwrap().to_owned();
        let weak_store = Arc::downgrade(&store);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let cancellation = Cancellation::default();
        let cancelled = cancellation.clone();
        let root = scenario.root.path.clone();
        *store.graph_validation_hook.lock().unwrap() = Some(Arc::new(move |_| {
            if observed.fetch_add(1, Ordering::Relaxed) == 2 {
                if fault == "cancel" {
                    cancelled.cancel();
                    return;
                }
                if fault == "root" {
                    std::fs::write(&root, line("rewritten-root")).unwrap();
                    return;
                }
                if fault == "append" {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&root)
                        .unwrap()
                        .write_all(line("appended-root").as_bytes())
                        .unwrap();
                    return;
                }
                let store = weak_store.upgrade().unwrap();
                let state = store.lock_state();
                let mut graph = state.prepared_graphs[&id].lock().unwrap();
                match fault {
                    "context" => {
                        graph.claimant.clear();
                        graph.claimant.push_str("other-owner");
                    }
                    "revision" => {
                        graph.revision.clear();
                        graph.revision.push_str("other-revision");
                    }
                    "expiry" => graph.expires = now_ms().saturating_sub(1),
                    _ => unreachable!(),
                }
            }
        }));
        let response = store.request(
            &graph_query(&graph, marker_tool(), json!([])),
            &owner,
            &cancellation,
        );
        let expected = match fault {
            "cancel" => "cancelled",
            "root" | "append" => "changed",
            _ => "stale_handle",
        };
        assert_eq!(
            response["status"].as_str(),
            Some(expected),
            "{fault}: {response:?}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        let state = store.lock_state();
        if let Some(shared) = state
            .prepared_graphs
            .get(graph["graph_id"].as_str().unwrap())
        {
            assert!(
                !shared.lock().unwrap().validated,
                "{fault}: refused validation was published"
            );
        }
        assert!(state.retained_graph_input_owners.is_empty());
        drop(state);
        assert_eq!(ledger(&store), audited(&store));
    }
}

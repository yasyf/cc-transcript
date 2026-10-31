#[test]
fn raw_tail_projection_matches_full_parse_and_shares_one_raw_owner() {
    for count in [32, 4096] {
        let initial = format!("{}\n", user("first"));
        let source = Source::new(&initial);
        let store = store();
        let owner = context("raw-tail");
        let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let mut added = String::new();
        for index in 0..count {
            added.push_str(&user(&format!("event-{index}")));
            added.push_str("\r\n");
            if index % 13 == 0 {
                added.push_str("\nnot-json\n[]\n");
            }
        }
        added.push_str(&interaction_call("prior", "same", "AskUserQuestion", r#"{"questions":[]}"#));
        added.push('\n');
        added.push_str(&interaction_call("replacement", "same", "Bash", r#"{"command":"new","opaque":{"retained":"yes"}}"#));
        source.append(&added);
        let expected = crate::parse::parse_bytes((initial + &added).as_bytes(), |_| true).unwrap();
        let expected = &expected[expected.len().saturating_sub(256)..];
        let mut read_usage = [0; 20];
        let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut read_usage).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&reads);
        *store.read_hook.lock().unwrap() = Some(Arc::new(move || { observed.fetch_add(1, Ordering::Relaxed); }));
        let mut usage = [0; 20];
        let tail = store.raw_tail_from_scope(handle(&response), &raw, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
        let tail_wire = sonic_rs::to_string(&tail.entries()).unwrap();
        assert_eq!(tail_wire, sonic_rs::to_string(expected).unwrap());
        assert_eq!((usage[0], usage[1]), (0, 0));
        assert!(usage[3] <= 320, "{usage:?}");
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert!(!tail.chunks[0].entries.arenas().is_empty());
        let interactions = store.indexed_interactions(Arc::clone(&tail), &owner, &Cancellation::default(), raw_scope_bounds()).unwrap();
        let winner = interactions.winner("same").unwrap();
        assert_eq!(winner.first, tail.event_count - 2);
        assert_eq!(winner.event, tail.event_count - 1);
        let mut reservation = store.reserve_projection(&owner, 0).unwrap();
        assert_eq!(interactions.call(winner, &mut reservation, &owner).unwrap().name, "Bash");
        drop((reservation, interactions));
        let mut warm_usage = [0; 20];
        let warm = store.raw_tail_from_scope(handle(&response), &raw, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
        assert!(Arc::ptr_eq(&tail, &warm));
        assert_eq!(&warm_usage[..4], &[0, 0, 0, 0]);
        let excerpt = store.raw_excerpt_from_scope(handle(&response), &raw, &[b"replacement"], 0, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
        assert_eq!(excerpt.event_count, 1);
        assert_eq!((warm_usage[0], warm_usage[1]), (0, 0));
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        {
            let state = store.lock_state();
            for scope in [&tail, &excerpt] {
                assert!(Arc::ptr_eq(state.generations.get(&snapshot_key(scope)).unwrap().raw_parent.as_ref().unwrap(), &raw.snapshot));
            }
            assert!(Arc::ptr_eq(state.generations.get(&snapshot_key(&raw.snapshot)).unwrap().raw_window.as_ref().unwrap(), &raw.window));
        }
        store.assert_conserved();
        source.append(&format!("\n{}\n", user("No")));
        let current = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut read_usage).unwrap();
        assert!(reads.load(Ordering::Relaxed) > 0);
        let current_tail = store.raw_tail_from_scope(handle(&response), &current, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
        assert!(!sonic_rs::to_string(&tail.entries()).unwrap().contains(r#""uuid":"No""#));
        assert!(sonic_rs::to_string(&current_tail.entries()).unwrap().contains(r#""uuid":"No""#));
        store.lock_state().generations.get_mut(&snapshot_key(&raw.source)).unwrap().raw_cache = None;
        drop((raw, current));
        assert_eq!(sonic_rs::to_string(&tail.entries()).unwrap(), tail_wire);
        store.lock_state().drain();
        store.assert_conserved();
        assert!(store.lock_state().generations.values().any(|record| record.raw_window.is_some()));
        eprintln!("raw_tail_projection corpus_events={count} decoded_lines={} source_bytes=0 warm_decoded_lines=0", usage[3]);
        drop((tail, warm, excerpt, current_tail));
        store.lock_state().drain();
        store.assert_conserved();
        assert!(store.lock_state().generations.values().all(|record| record.raw_window.is_none()));
    }
}

#[test]
fn raw_tail_projection_refuses_missing_coverage_and_keeps_pinned_scopes() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("raw-tail-coverage");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    for index in 0..20 { source.append(&format!("{}\n", user(&format!("event-{index}")))); }
    let mut usage = [0; 20];
    let short = store.raw_source_scope(handle(&response), 512, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert!(short.snapshot.window_start > 0);
    usage = [0; 20];
    let failure = store.raw_tail_from_scope(handle(&response), &short, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::SourceLimit);
    assert_eq!((usage[0], usage[1]), (0, 0));
    assert!(store.lock_state().generations.get(&snapshot_key(&short.source)).unwrap().raw_cache.as_ref().unwrap().tail.is_none());
    let old_tail = store.raw_tail_from_scope(handle(&response), &short, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let full = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(full.snapshot.window_start, 0);
    let full_tail = store.raw_tail_from_scope(handle(&response), &full, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(full_tail.event_count, 21);
    usage = [0; 20];
    let old_again = store.raw_tail_from_scope(handle(&response), &short, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(sonic_rs::to_string(&old_tail.entries()).unwrap(), sonic_rs::to_string(&old_again.entries()).unwrap());
    assert_eq!((usage[0], usage[1]), (0, 0));
    {
        let state = store.lock_state();
        let cache = state.generations.get(&snapshot_key(&full.source)).unwrap().raw_cache.as_ref().unwrap();
        assert!(Arc::ptr_eq(&cache.raw, &full.snapshot));
        assert!(Arc::ptr_eq(&cache.tail.as_ref().unwrap().snapshot, &full_tail));
    }
    usage = [0; 20];
    let empty = store.raw_tail_from_scope(handle(&response), &short, 0, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(empty.event_count, 0);
    assert_eq!(&usage[..4], &[0, 0, 0, 0]);
    source.append("{\"type\":\"user\"");
    let partial = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let complete = store.raw_tail_from_scope(handle(&response), &partial, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(complete.event_count, 21);
    store.assert_conserved();
}

#[test]
fn raw_tail_projection_enforces_origin_lease_window_and_current_authority() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("raw-tail-origin");
    let first = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    source.append(&format!("{}\n", user("second")));
    let mut usage = [0; 20];
    let raw = store.raw_source_scope(handle(&first), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let second = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    usage = [0; 20];
    let failure = store.raw_tail_from_scope(handle(&second), &raw, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::StaleHandle);
    let mut denied = owner.clone();
    denied.insert("authority", json!({"kind":"restricted_roots","roots":[],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
    let failure = store.raw_tail_from_scope(handle(&first), &raw, 256, &denied, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::PermissionDenied);
    let mut registry = owner.clone();
    registry.insert("registry_generation", json!("different"));
    let failure = store.raw_tail_from_scope(handle(&first), &raw, 256, &registry, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::StaleHandle);
    let unpaired = RawSourceScope { source: Arc::clone(&raw.source), snapshot: Arc::clone(&raw.snapshot), window: Arc::new(crate::snapshot_raw_window::RawWindow::from_line_boundary_bytes(Vec::new())) };
    let failure = store.raw_tail_from_scope(handle(&first), &unpaired, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::StaleHandle);
    assert_eq!(&usage[..4], &[0, 0, 0, 0]);
    store.assert_conserved();
}

#[test]
fn raw_tail_projection_rolls_back_work_budget_and_typed_parse_failures() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("raw-tail-errors");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    source.append(&format!("{}\n", user("second")));
    let mut usage = [0; 20];
    let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let transient = store.lock_state().transient_bytes;
    let mut bounds = raw_scope_bounds();
    bounds.max_read_bytes = 1;
    let failure = store.raw_tail_from_scope(handle(&response), &raw, 256, &owner, &Cancellation::default(), bounds, &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::SourceLimit);
    assert_eq!(store.lock_state().transient_bytes, transient);
    bounds = raw_scope_bounds();
    bounds.max_events = 1;
    let failure = store.raw_tail_from_scope(handle(&response), &raw, 256, &owner, &Cancellation::default(), bounds, &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::EntryLimit);
    assert_eq!(store.lock_state().transient_bytes, transient);
    source.append("{\"type\":\"user\"}\n");
    let malformed = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let failure = store.raw_tail_from_scope(handle(&response), &malformed, 256, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).err().unwrap();
    assert_eq!(failure.status, Status::ParseError);
    assert_eq!(store.lock_state().transient_bytes, transient);
    assert!(store.lock_state().generations.get(&snapshot_key(&raw.source)).unwrap().raw_cache.as_ref().unwrap().tail.is_none());
    store.assert_conserved();
}

#[test]
fn raw_tail_projection_session_refs_follow_first_selected_event_metadata() {
    for selected_session in ["tail-session".to_owned(), String::new(), "copied-🦀".to_owned(), "long-session-".repeat(256)] {
        let initial = format!("{}\n", user("base").replace(r#""sessionId":"s""#, r#""sessionId":"base-session""#));
        let source = Source::new(&initial);
        let store = store();
        let owner = context("raw-selected-session");
        let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
        let selected_user = user("selected-user").replace(r#""sessionId":"s""#, &format!(r#""sessionId":"{selected_session}""#));
        let call = interaction_call("selected-call", "same", "Bash", r#"{"command":"true"}"#)
            .replace(r#""sessionId":"s""#, r#""sessionId":"call-session""#);
        source.append(&format!("{{\"type\":\"mode\",\"sessionId\":\"wrong-mode-session\",\"mode\":\"plan\"}}\n{selected_user}\n{call}\n"));
        let mut usage = [0; 20];
        let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
        assert_eq!(raw.snapshot.session_id, "base-session");
        let tail = store.raw_tail_from_scope(handle(&response), &raw, 3, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
        let excerpt = store.raw_excerpt_from_scope(handle(&response), &raw, &[b"selected-call"], 0, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
        for (scope, expected_session) in [(&tail, selected_session.as_str()), (&excerpt, "call-session")] {
            assert_eq!(scope.session_id, expected_session);
            let index = store.indexed_interactions(Arc::clone(scope), &owner, &Cancellation::default(), raw_scope_bounds()).unwrap();
            let mut reservation = store.reserve_projection(&owner, 0).unwrap();
            let selected = index.call(index.winner("same").unwrap(), &mut reservation, &owner).unwrap();
            let actual = crate::snapshot_codec::ToolUseWire::new(&selected, &scope.session_id);
            let expected = crate::snapshot_codec::ToolUseWire::new(&selected, expected_session);
            assert_eq!(actual.r#ref.session_id, expected_session);
            assert_eq!(crate::snapshot_codec::encode(&actual, 64*1024).unwrap(), crate::snapshot_codec::encode(&expected, 64*1024).unwrap());
        }
        store.assert_conserved();
    }
}

#[test]
fn raw_tail_projection_session_falls_back_to_stem_without_event_metadata() {
    let source = Source::new(&format!("{}\n", user("base").replace(r#""sessionId":"s""#, r#""sessionId":"base-session""#)));
    let store = store();
    let owner = context("raw-mode-session");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    source.append("{\"type\":\"mode\",\"sessionId\":\"wrong-mode-session\",\"mode\":\"plan\"}\n");
    let mut usage = [0; 20];
    let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let tail = store.raw_tail_from_scope(handle(&response), &raw, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let excerpt = store.raw_excerpt_from_scope(handle(&response), &raw, &[b"wrong-mode-session"], 0, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let expected = source.path.file_stem().unwrap().to_string_lossy();
    for scope in [&tail, &excerpt] {
        assert!(scope.entries().iter().all(|entry| entry.meta().is_none()));
        assert_eq!(scope.session_id, expected.as_ref());
        assert_ne!(scope.session_id, "wrong-mode-session");
    }
    store.assert_conserved();
}

#[test]
fn raw_tail_projection_refuses_current_route_escape_before_work_and_publication() {
    for excerpt in [false, true] {
        for late in [false, true] {
            let mut source = Source::new(&format!("{}\n", user("first")));
            let allowed = source.directory.join("allowed");
            let nested = allowed.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            let path = nested.join("s.jsonl");
            std::fs::rename(&source.path, &path).unwrap();
            source.path = path;
            let store = store();
            let mut owner = context("raw-route");
            owner.insert("authority", json!({"kind":"restricted_roots","roots":[allowed.to_string_lossy().as_ref()],"effective_uid":unsafe {libc::geteuid()}.to_string()}));
            let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
            let mut usage = [0; 20];
            let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
            let stamp = raw.snapshot.stamp;
            let outside = source.directory.join("outside");
            let moved = Arc::new(AtomicBool::new(false));
            if late {
                let original = nested.clone();
                let target = outside.clone();
                let observed = Arc::clone(&moved);
                *store.decode_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                    if stage == "entry" && !observed.swap(true, Ordering::Relaxed) {
                        std::fs::rename(&original, &target).unwrap();
                        std::os::unix::fs::symlink(&target, &original).unwrap();
                    }
                }));
            } else {
                std::fs::rename(&nested, &outside).unwrap();
                std::os::unix::fs::symlink(&outside, &nested).unwrap();
                moved.store(true, Ordering::Relaxed);
            }
            usage = [0; 20];
            let transient = store.lock_state().transient_bytes;
            let result = if excerpt {
                store.raw_excerpt_from_scope(handle(&response), &raw, &[b"first"], 0, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage)
            } else {
                store.raw_tail_from_scope(handle(&response), &raw, 1, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage)
            };
            assert_eq!(result.err().unwrap().status, Status::PermissionDenied);
            assert!(moved.load(Ordering::Relaxed));
            assert_eq!(SourceStamp::of(&std::fs::metadata(&source.path).unwrap()), stamp);
            assert_eq!((usage[0], usage[1]), (0, 0));
            if !late { assert_eq!((usage[2], usage[3]), (0, 0)); }
            assert_eq!(store.lock_state().transient_bytes, transient);
            let state = store.lock_state();
            let cache = state.generations.get(&snapshot_key(&raw.source)).unwrap().raw_cache.as_ref().unwrap();
            assert!(cache.tail.is_none() && cache.excerpt.is_none());
            drop(state);
            store.assert_conserved();
        }
    }
}

#[test]
fn raw_tail_projection_rejects_last_decode_cancel_and_deadline_without_publication() {
    for excerpt in [false, true] {
        for deadline in [false, true] {
            let source = Source::new(&format!("{}\n", user("first")));
            let store = store();
            let owner = context("raw-final-cancel");
            let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
            let mut usage = [0; 20];
            let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
            let cancel = Cancellation::default();
            let observed_cancel = cancel.clone();
            let reached = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&reached);
            let mut bounds = raw_scope_bounds();
            if deadline { bounds.deadline_unix_ms = now_ms() + 2000; }
            let expires = bounds.deadline_unix_ms;
            *store.decode_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                if stage == "entry" && !observed.swap(true, Ordering::Relaxed) {
                    if deadline {
                        std::thread::sleep(std::time::Duration::from_millis(expires.saturating_sub(now_ms()) + 1));
                    } else {
                        observed_cancel.cancel();
                    }
                }
            }));
            usage = [0; 20];
            let transient = store.lock_state().transient_bytes;
            let result = if excerpt {
                store.raw_excerpt_from_scope(handle(&response), &raw, &[b"first"], 0, 1, &owner, &cancel, bounds, &mut usage)
            } else {
                store.raw_tail_from_scope(handle(&response), &raw, 1, &owner, &cancel, bounds, &mut usage)
            };
            assert!(reached.load(Ordering::Relaxed));
            assert_eq!(result.err().unwrap().status, if deadline { Status::Deadline } else { Status::Cancelled });
            assert_eq!((usage[0], usage[1]), (0, 0));
            assert_eq!(store.lock_state().transient_bytes, transient);
            let state = store.lock_state();
            if let Some(cache) = state.generations.get(&snapshot_key(&raw.source)).unwrap().raw_cache.as_ref() {
                assert!(cache.tail.is_none() && cache.excerpt.is_none());
            }
            drop(state);
            store.assert_conserved();
        }
    }
}

fn finish_shared_publication_fixture(
    store: &NativeStore,
    mut response: Value,
    owner: &Value,
) -> (Value, u64) {
    let mut read = 0;
    for _ in 0..256 {
        read += response["usage"]["source_bytes_read"].as_u64().unwrap();
        store.assert_conserved();
        if response["status"].as_str() != Some("incomplete") {
            return (response, read);
        }
        response = store.request(
            &json!({"schema":SCHEMA,"id":"shared-publication-resume","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
            owner,
            &Cancellation::default(),
        );
    }
    panic!("shared publication fixture did not finish")
}

#[test]
fn shared_publication_admission_counts_raw_allocations_once_and_reuses_source() {
    let payload = "result payload ".repeat(8192);
    let line = format!(
        "{}\n",
        json!({
            "type":"user","uuid":"large-result","sessionId":"s","timestamp":"2026-01-02T03:04:05Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":payload}]}
        })
    );
    let source = Source::new(&line);
    let cap = 64 * 1024 * 1024;
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step":8192,"max_events_per_step":32,"max_entry_bytes":1024*1024,
        "max_retained_bytes":cap,"reserved_hook_accounted_bytes":4096
    }))
    .unwrap();
    let mut owner = context("shared-publication-admission");
    owner["admission"] = json!("review");
    let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    let mut first_read = 0;
    let mut ready = false;
    for _ in 0..128 {
        first_read += response["usage"]["source_bytes_read"].as_u64().unwrap();
        assert_eq!(
            response["status"].as_str(),
            Some("incomplete"),
            "{response:?}"
        );
        let slot = store.lock_state().loads.values().next().cloned().unwrap();
        let load = slot.work.lock().unwrap();
        if load.decoded && load.indexed == load.count && load.count > 0 {
            assert!(!load.sealed);
            assert!(load.result.is_none());
            ready = true;
            break;
        }
        drop(load);
        response = store.request(
            &json!({"schema":SCHEMA,"id":"shared-publication-prime","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
            &owner,
            &Cancellation::default(),
        );
    }
    assert!(ready);
    let headroom = 96 * 1024;
    let crowded = store
        .reserve_projection(
            &owner,
            cap - 4096 - store.retained_accounted_bytes() - headroom,
        )
        .unwrap();
    response = store.request(
        &json!({"schema":SCHEMA,"id":"shared-publication-seal","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
        &owner,
        &Cancellation::default(),
    );
    let (first, read) = finish_shared_publication_fixture(&store, response, &owner);
    first_read += read;
    assert_eq!(
        first["status"].as_str(),
        Some("ok"),
        "shared raw publication admission refused: {first:?}"
    );
    assert_eq!(first_read, line.len() as u64 + 128);
    let handle = &first["data"]["description"]["handle"];
    let snapshot = store
        .renew_scope_for_work(handle, &owner, now_ms() + 30_000)
        .unwrap()
        .snapshot;
    let Entry::User(result_user) = &snapshot.chunks[0].entries[0] else {
        panic!("result entry changed")
    };
    assert_eq!(result_user.tool_results().next().unwrap().content, payload);
    assert_eq!(snapshot.activity.entry_count(), 1);
    for _ in 0..2 {
        let (repeated, read) = finish_shared_publication_fixture(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(repeated["status"].as_str(), Some("ok"), "{repeated:?}");
        assert_eq!(read, 0);
        assert_eq!(
            repeated["data"]["description"]["handle"]["generation"],
            first["data"]["description"]["handle"]["generation"]
        );
        assert_eq!(
            repeated["data"]["description"]["mtime_ns"],
            first["data"]["description"]["mtime_ns"]
        );
    }
    let appended = format!("{}\n", user("appended-owner-words"));
    source.append(&appended);
    let (grown, append_read) = finish_shared_publication_fixture(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(grown["status"].as_str(), Some("ok"), "{grown:?}");
    assert_eq!(
        grown["data"]["description"]["event_count"].as_u64(),
        Some(2)
    );
    assert_eq!(append_read, appended.len() as u64 + 192);
    eprintln!("SHARED_PUBLICATION_ADMISSION source_bytes={} first_read_bytes={} unchanged_calls=2 unchanged_read_bytes=0 event_count=1 payload_bytes={} headroom_bytes={headroom} append_bytes={} append_read_bytes={append_read}", line.len(), first_read, payload.len(), appended.len());
    drop(crowded);
    store.assert_conserved();
}

#[test]
fn shared_publication_admission_reuses_219mb_source_with_other_generations() {
    let source = Source::new("");
    let target_bytes = 219_081_957;
    let payload = "r".repeat(16 * 1024);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&source.path)
        .unwrap();
    let mut written = 0;
    let mut count = 0;
    loop {
        let row = format!(
            "{}\n",
            json!({
                "type":"user","uuid":format!("result-{count}"),"sessionId":"s",
                "timestamp":"2026-01-02T03:04:05Z","isSidechain":true,
                "toolUseResult":{"output":payload},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":format!("call-{count}"),"content":payload}]}
            })
        );
        if written + row.len() >= target_bytes {
            break;
        }
        file.write_all(row.as_bytes()).unwrap();
        written += row.len();
        count += 1;
    }
    file.write_all(" ".repeat(target_bytes - written - 1).as_bytes())
        .unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    assert_eq!(
        std::fs::metadata(&source.path).unwrap().len(),
        target_bytes as u64
    );
    let cap = 1408 * 1024 * 1024;
    let reserved = 512 * 1024 * 1024;
    let store = NativeStore::new(&json!({
        "max_read_bytes_per_step":8*1024*1024,"max_events_per_step":100_000,
        "max_entry_bytes":8*1024*1024,"max_source_bytes":512*1024*1024,
        "max_retained_bytes":cap,"reserved_hook_accounted_bytes":reserved
    }))
    .unwrap();
    let mut owner = context("shared-publication-219mb");
    owner["admission"] = json!("review");
    let prefixes = [Source::new(&users(0..32)), Source::new(&users(32..64))];
    let pins: Vec<_> = prefixes
        .iter()
        .map(|prefix| {
            let (response, _) = finish_shared_publication_fixture(
                &store,
                store.request(&acquire(&prefix.path), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            store
                .pin(&response["data"]["description"]["handle"], &owner)
                .unwrap()
        })
        .collect();
    let mut request = acquire(&source.path);
    request["limits"]["max_read_bytes"] = json!(512 * 1024 * 1024);
    request["limits"]["max_source_read_bytes"] = json!(512 * 1024 * 1024);
    request["limits"]["max_events"] = json!(1_000_000);
    request["deadline_unix_ms"] = json!(now_ms() + 120_000);
    let canonical_source = std::fs::canonicalize(&source.path).unwrap();
    let mut response = store.request(&request, &owner, &Cancellation::default());
    let mut first_read = 0;
    let mut ready = false;
    for _ in 0..256 {
        first_read += response["usage"]["source_bytes_read"].as_u64().unwrap();
        assert_eq!(
            response["status"].as_str(),
            Some("incomplete"),
            "{response:?}"
        );
        let slot = store
            .lock_state()
            .loads
            .values()
            .find(|slot| slot.path == canonical_source)
            .cloned()
            .unwrap();
        let load = slot.work.lock().unwrap();
        if load.decoded && load.indexed == load.count && load.count > 0 {
            assert!(!load.sealed);
            assert!(load.result.is_none());
            ready = true;
            break;
        }
        drop(load);
        response = store.request(
            &json!({"schema":SCHEMA,"id":"shared-publication-219mb-prime","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
            &owner, &Cancellation::default(),
        );
    }
    assert!(ready);
    let headroom = 64 * 1024 * 1024;
    let crowded = store
        .reserve_projection(
            &owner,
            cap - reserved - store.retained_accounted_bytes() - headroom,
        )
        .unwrap();
    response = store.request(
        &json!({"schema":SCHEMA,"id":"shared-publication-219mb-seal","operation":"resume","cursor":response["cursor"].as_str().unwrap()}),
        &owner, &Cancellation::default(),
    );
    let (first, read) = finish_shared_publication_fixture(&store, response, &owner);
    first_read += read;
    assert_eq!(
        first["status"].as_str(),
        Some("ok"),
        "shared raw publication admission refused: {first:?}"
    );
    assert_eq!(first_read, target_bytes as u64 + 128);
    let snapshot = store
        .pin(&first["data"]["description"]["handle"], &owner)
        .unwrap();
    assert_eq!(snapshot.event_count, count);
    let Entry::User(result_user) = snapshot.entry(count - 1) else {
        panic!("result entry changed")
    };
    assert_eq!(result_user.tool_results().next().unwrap().content, payload);
    for _ in 0..2 {
        let (repeated, read) = finish_shared_publication_fixture(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(repeated["status"].as_str(), Some("ok"), "{repeated:?}");
        assert_eq!(read, 0);
        assert_eq!(
            repeated["data"]["description"]["handle"]["generation"],
            first["data"]["description"]["handle"]["generation"]
        );
    }
    eprintln!("SHARED_PUBLICATION_ADMISSION source_bytes={target_bytes} first_read_bytes={first_read} unchanged_calls=2 unchanged_read_bytes=0 event_count={count} payload_bytes={} headroom_bytes={headroom} prefix_generations={}", payload.len(), pins.len());
    drop(crowded);
    drop(pins);
    store.assert_conserved();
}

fn sidechain_feed_store() -> NativeStore {
    NativeStore::new(&json!({"max_events_per_step":4096,"max_retained_bytes":256*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":64,"reserved_hook_leases":1})).unwrap()
}

fn sidechain_session(count: usize) -> (Source, PathBuf) {
    let source = Source::new(&format!("{}\n", user("root")));
    let directory = source.directory.join("s/subagents");
    std::fs::create_dir_all(&directory).unwrap();
    for index in 0..count {
        std::fs::write(
            directory.join(format!("agent-{index:04}.jsonl")),
            format!("{}\n", user(&format!("agent-{index}"))),
        )
        .unwrap();
    }
    (source, directory)
}

fn fed_root(store: &NativeStore, source: &Source, owner: &Value) -> Value {
    finish(
        store,
        store.request(&acquire(&source.path), owner, &Cancellation::default()),
        owner,
    )
}

fn fed_prepare(
    store: &NativeStore,
    root: &Value,
    owner: &Value,
    count: usize,
) -> (Value, SidechainCalls) {
    let mut request = prepare_request(root);
    request["limits"]["max_sources"] = json!(count + 2);
    request["limits"]["max_discovery_entries"] = json!(count + 16);
    SIDECHAIN_CALLS.set(SidechainCalls::default());
    let prepared = finish_prepared(
        store,
        store.request(&request, owner, &Cancellation::default()),
        owner,
    );
    assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
    (prepared, SIDECHAIN_CALLS.get())
}

fn fed_sources(store: &NativeStore, prepared: &Value) -> Vec<(PathBuf, SourceStamp)> {
    store.lock_state().prepared_graphs[prepared["data"]["handle"]["graph_id"].as_str().unwrap()]
        .lock()
        .unwrap()
        .sources
        .iter()
        .map(|source| (source.path.clone(), source.stamp))
        .collect()
}

fn assert_repeat_prepare_makes_no_sidechain_calls(count: usize) {
    raise_descriptor_limit();
    let (source, _directory) = sidechain_session(count);
    let store = sidechain_feed_store();
    let owner = context("feed");
    let root = fed_root(&store, &source, &owner);
    let watching = count + 1;
    let (first, registered) = fed_prepare(&store, &root, &owner, count);
    assert_eq!(fed_sources(&store, &first).len(), count);
    assert_eq!(
        registered,
        SidechainCalls {
            listings: 1,
            resolutions: count + 1,
            opens: watching,
            stats: count + 2,
        },
        "the first prepare at {count} sidechains"
    );
    let (second, repeated) = fed_prepare(&store, &root, &owner, count);
    assert_eq!(
        repeated,
        SidechainCalls::default(),
        "a repeat prepare at {count} sidechains"
    );
    assert_eq!(fed_sources(&store, &second), fed_sources(&store, &first));
    assert_eq!(store.sidechain_feed.descriptors(), watching);
    store.lock_state().retain_latest(|_, _| false);
    store.sidechain_feed.drain();
    assert_eq!(
        store.sidechain_feed.descriptors(),
        0,
        "the session's watches outlived it"
    );
}

#[test]
fn repeat_prepare_graph_makes_no_sidechain_calls_at_one_sidechain() {
    assert_repeat_prepare_makes_no_sidechain_calls(1);
}

#[test]
fn repeat_prepare_graph_makes_no_sidechain_calls_at_a_thousand_sidechains() {
    assert_repeat_prepare_makes_no_sidechain_calls(1000);
}

#[test]
fn a_sidechain_created_after_a_prepare_joins_the_next_graph() {
    let (source, directory) = sidechain_session(1);
    let store = sidechain_feed_store();
    let owner = context("feed");
    let root = fed_root(&store, &source, &owner);
    fed_prepare(&store, &root, &owner, 2);
    let late = directory.join("agent-late.jsonl");
    std::fs::write(&late, format!("{}\n", user("late"))).unwrap();
    let (prepared, _) = fed_prepare(&store, &root, &owner, 2);
    let late = std::fs::canonicalize(&late).unwrap();
    assert!(
        fed_sources(&store, &prepared)
            .iter()
            .any(|(path, _)| *path == late),
        "the new sidechain is missing from the next graph"
    );
}

#[test]
fn a_sidechain_written_after_a_prepare_pins_its_new_stamp_and_queries_cleanly() {
    let (source, directory) = sidechain_session(2);
    let store = sidechain_feed_store();
    let owner = context("feed");
    let root = fed_root(&store, &source, &owner);
    let (first, _) = fed_prepare(&store, &root, &owner, 2);
    let written = std::fs::canonicalize(directory.join("agent-0000.jsonl")).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&written)
        .unwrap()
        .write_all(format!("{}\n", user("agent-0-later")).as_bytes())
        .unwrap();
    let (second, _) = fed_prepare(&store, &root, &owner, 2);
    let pinned = |prepared: &Value| {
        fed_sources(&store, prepared)
            .into_iter()
            .find(|(path, _)| *path == written)
            .unwrap()
            .1
    };
    assert_ne!(pinned(&second), pinned(&first));
    assert_eq!(
        pinned(&second),
        SourceStamp::of(&std::fs::metadata(&written).unwrap())
    );
    let template = acquire(&source.path);
    let answered = finish_prepared(
        &store,
        store.request(
            &json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":second["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    assert_eq!(answered["status"].as_str(), Some("ok"), "{answered:?}");
}

#[test]
fn an_unwatchable_sidechain_keeps_the_stamp_check_and_warns_once() {
    let (source, directory) = sidechain_session(2);
    let outside = source.directory.join("outside.jsonl");
    std::fs::write(&outside, format!("{}\n", user("outside"))).unwrap();
    let linked = directory.join("agent-linked.jsonl");
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    let store = sidechain_feed_store();
    let owner = context("feed");
    let root = fed_root(&store, &source, &owner);
    fed_prepare(&store, &root, &owner, 3);
    assert_eq!(
        store
            .take_sidechain_watch_failures()
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec![std::fs::canonicalize(&directory)
            .unwrap()
            .join("agent-linked.jsonl")]
    );
    let (prepared, repeated) = fed_prepare(&store, &root, &owner, 3);
    assert_eq!(
        (repeated.resolutions, repeated.stats),
        (1, 1),
        "the unwatched sidechain skipped its stamp check"
    );
    assert!(store.take_sidechain_watch_failures().is_empty());
    let outside = std::fs::canonicalize(&outside).unwrap();
    assert!(fed_sources(&store, &prepared)
        .iter()
        .any(|(path, _)| *path == outside));
}

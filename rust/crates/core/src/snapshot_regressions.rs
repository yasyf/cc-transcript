use super::*;
use std::sync::{mpsc, Condvar};
use std::time::Duration;

struct RaceSource {
    directory: PathBuf,
    path: PathBuf,
}

impl RaceSource {
    fn new(contents: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "cc-snapshot-races-{}-{}-{}",
            std::process::id(),
            now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("session.jsonl");
        std::fs::write(&path, contents).unwrap();
        Self { directory, path }
    }

    fn append(&self, contents: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
    }

    fn rewrite(&self, contents: &str) {
        let before = SourceStamp::of(&std::fs::metadata(&self.path).unwrap());
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        let after = SourceStamp::of(&std::fs::metadata(&self.path).unwrap());
        assert_eq!(before.identity, after.identity);
    }
}

impl Drop for RaceSource {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn user(id: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":id,"sessionId":"s",
        "timestamp":"2026-01-02T03:04:05Z","message":{"content":"request ".repeat(32)}})
    )
}

fn race_store() -> NativeStore {
    NativeStore::new(
        &json!({"max_read_bytes_per_step":128,"max_events_per_step":2,
        "max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,
        "reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1}),
    )
    .unwrap()
}

fn context(store: &NativeStore, source: &RaceSource, claimant: &str) -> Value {
    json!({"claimant":claimant,"admission":"hook","authority":{"kind":"user",
        "effective_uid":std::fs::metadata(&source.directory).unwrap().uid().to_string()},
        "registry_generation":store.default_registry_generation()})
}

fn bounds() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 1024 * 1024,
        max_events: 1000,
        max_items: 256,
        max_output_bytes: 1024 * 1024,
        max_discovery_entries: 1000,
        max_sources: 100,
        deadline_unix_ms: now_ms() + 30_000,
    }
}

fn acquire(path: &Path) -> Value {
    json!({"schema":SCHEMA,"id":"race-acquire","operation":"acquire",
        "path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},
        "deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_events":1000,
        "max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
}

fn resume(store: &NativeStore, response: &Value, owner: &Value, cancel: &Cancellation) -> Value {
    assert_eq!(
        response["status"].as_str(),
        Some("incomplete"),
        "{response:?}"
    );
    store.request(
        &json!({"schema":SCHEMA,"id":"race-resume","operation":"resume",
        "cursor":response["cursor"]}),
        owner,
        cancel,
    )
}

fn finish(store: &NativeStore, mut response: Value, owner: &Value) -> Value {
    for _ in 0..100 {
        if response["status"].as_str() != Some("incomplete") {
            return response;
        }
        response = resume(store, &response, owner, &Cancellation::default());
    }
    panic!("bounded source did not finish");
}

fn pin(store: &NativeStore, response: &Value, owner: &Value) -> Arc<TranscriptSnapshot> {
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    store
        .pin(&response["data"]["description"]["handle"], owner)
        .unwrap()
}

fn staged_body(store: &NativeStore, mut response: Value, owner: &Value, after: u64) -> Value {
    for _ in 0..100 {
        assert_eq!(
            response["status"].as_str(),
            Some("incomplete"),
            "{response:?}"
        );
        let slot = {
            let state = store.state.lock().unwrap();
            Arc::clone(&state.waiters[response["cursor"].as_str().unwrap()].load)
        };
        let ready = {
            let load = slot.work.lock().unwrap();
            load.origin_complete
                && (load.previous.is_none() || load.prefix_checked)
                && load.indexed == load.count
                && !load.decoded
                && load.offset > after
                && load.offset < slot.stamp.size
                && (load.pending.is_empty() || !load.pending.contains(&b'\n'))
        };
        if ready {
            return response;
        }
        response = resume(store, &response, owner, &Cancellation::default());
    }
    panic!("source never reached a staged body read");
}

struct ReadGate {
    entered: mpsc::Receiver<()>,
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl ReadGate {
    fn wait_until_read(&self) {
        self.entered
            .recv_timeout(Duration::from_secs(10))
            .expect("source read hook was not reached within ten seconds");
    }

    fn release(&self) {
        let (released, notification) = &*self.released;
        *released.lock().unwrap() = true;
        notification.notify_all();
    }
}

impl Drop for ReadGate {
    fn drop(&mut self) {
        self.release();
    }
}

fn install_read_gate(store: &NativeStore) -> ReadGate {
    let (sender, entered) = mpsc::channel();
    let released = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_released = Arc::clone(&released);
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
        sender.send(()).expect("read gate owner disappeared");
        let (released, notification) = &*worker_released;
        let (released, _) = notification
            .wait_timeout_while(
                released.lock().unwrap(),
                Duration::from_secs(10),
                |released| !*released,
            )
            .unwrap();
        let did_release = *released;
        drop(released);
        assert!(did_release, "read gate was not released within ten seconds");
    }));
    ReadGate { entered, released }
}

#[test]
fn dropping_read_gate_releases_the_blocked_worker() {
    let store = race_store();
    let gate = install_read_gate(&store);
    let hook = store.read_hook.lock().unwrap().take().unwrap();
    let (finished, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        hook();
        finished.send(()).unwrap();
    });
    gate.wait_until_read();
    drop(gate);
    received
        .recv_timeout(Duration::from_secs(10))
        .expect("dropping the gate did not release its worker");
    worker.join().unwrap();
}

fn rewrite_during_read(tail: bool, truncate: bool) {
    let original = if tail {
        user("a")
    } else {
        [user("a"), user("b"), user("c")].concat()
    };
    let source = RaceSource::new(&original);
    let store = Arc::new(race_store());
    let owner = context(&store, &source, "reader");
    let prior = tail.then(|| {
        let complete = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        pin(&store, &complete, &owner)
    });
    if tail {
        source.append(&[user("b"), user("c"), user("d")].concat());
    }
    let preparation = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    let preparation = staged_body(
        &store,
        preparation,
        &owner,
        prior
            .as_ref()
            .map_or(0, |snapshot| snapshot.committed_bytes),
    );
    let before_publish = store.state.lock().unwrap().counters[9];
    let before_stamp = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    let gate = install_read_gate(&store);
    let worker_store = Arc::clone(&store);
    let worker_owner = owner.clone();
    let worker = std::thread::spawn(move || {
        resume(
            &worker_store,
            &preparation,
            &worker_owner,
            &Cancellation::default(),
        )
    });
    gate.wait_until_read();
    let replacement = if truncate {
        String::new()
    } else {
        let bytes = std::fs::read_to_string(&source.path).unwrap();
        bytes.replace("request", "changed")
    };
    source.rewrite(&replacement);
    assert_eq!(
        before_stamp.identity,
        SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity
    );
    if !truncate {
        assert_eq!(before_stamp.size, replacement.len() as u64);
    }
    gate.release();
    let failed = finish(&store, worker.join().unwrap(), &owner);
    assert_eq!(failed["status"].as_str(), Some("changed"), "{failed:?}");
    assert!(!failed["complete"].as_bool().unwrap());
    assert_eq!(store.state.lock().unwrap().counters[9], before_publish);
    if let Some(prior) = prior {
        assert_eq!(prior.event_count, 1);
        assert_eq!(prior.entry(0).meta().unwrap().uuid, "a");
        assert_eq!(prior.activity.turn_count(), 1);
    }
}

#[test]
fn same_inode_truncate_during_staged_cold_read_never_publishes() {
    rewrite_during_read(false, true);
}

#[test]
fn same_inode_rewrite_during_staged_cold_read_never_publishes() {
    rewrite_during_read(false, false);
}

#[test]
fn same_inode_truncate_during_staged_tail_read_keeps_old_generation() {
    rewrite_during_read(true, true);
}

#[test]
fn same_inode_rewrite_during_staged_tail_read_keeps_old_generation() {
    rewrite_during_read(true, false);
}

#[test]
fn concurrent_waiter_cancellation_does_not_cancel_the_blocked_reader() {
    let source = RaceSource::new(&[user("a"), user("b"), user("c")].concat());
    let store = Arc::new(race_store());
    let a = context(&store, &source, "first");
    let b = context(&store, &source, "second");
    let gate = install_read_gate(&store);
    let worker_store = Arc::clone(&store);
    let worker_owner = a.clone();
    let request = acquire(&source.path);
    let worker = std::thread::spawn(move || {
        worker_store.request(&request, &worker_owner, &Cancellation::default())
    });
    gate.wait_until_read();
    let second = store.request(&acquire(&source.path), &b, &Cancellation::default());
    let load_id = second["data"]["reservation"]["load_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(store.state.lock().unwrap().waiters.len(), 2);
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let failed = resume(&store, &second, &b, &cancellation);
    assert_eq!(failed["status"].as_str(), Some("cancelled"));
    assert_eq!(store.state.lock().unwrap().waiters.len(), 1);
    gate.release();
    let first = worker.join().unwrap();
    assert_eq!(
        first["data"]["reservation"]["load_id"].as_str(),
        Some(load_id.as_str())
    );
    let completed = finish(&store, first, &a);
    assert_eq!(pin(&store, &completed, &a).event_count, 3);
    let state = store.state.lock().unwrap();
    assert_eq!(state.counters[4], 1);
    assert_eq!(state.counters[8], 1);
    assert_eq!(state.counters[9], 1);
}

#[test]
fn cancelled_blocked_reader_does_not_poison_the_concurrent_waiter() {
    let source = RaceSource::new(&[user("a"), user("b"), user("c")].concat());
    let store = Arc::new(race_store());
    let a = context(&store, &source, "first");
    let b = context(&store, &source, "second");
    let cancellation = Cancellation::default();
    let gate = install_read_gate(&store);
    let worker_store = Arc::clone(&store);
    let worker_owner = a.clone();
    let worker_cancel = cancellation.clone();
    let request = acquire(&source.path);
    let worker =
        std::thread::spawn(move || worker_store.request(&request, &worker_owner, &worker_cancel));
    gate.wait_until_read();
    let second = store.request(&acquire(&source.path), &b, &Cancellation::default());
    assert_eq!(store.state.lock().unwrap().waiters.len(), 2);
    cancellation.cancel();
    gate.release();
    let failed = worker.join().unwrap();
    assert_eq!(failed["status"].as_str(), Some("cancelled"));
    assert_eq!(store.state.lock().unwrap().waiters.len(), 1);
    let completed = finish(&store, second, &b);
    assert_eq!(pin(&store, &completed, &b).event_count, 3);
    let state = store.state.lock().unwrap();
    assert_eq!(state.counters[4], 1);
    assert_eq!(state.counters[8], 1);
    assert_eq!(state.counters[9], 1);
}

fn classified(
    store: &NativeStore,
    snapshot: Arc<TranscriptSnapshot>,
    owner: &Value,
    classifier: &str,
) -> Arc<TranscriptSnapshot> {
    let mut usage = [0; 18];
    for _ in 0..100 {
        let progress = store
            .classify(
                Arc::clone(&snapshot),
                &json!({"id":classifier,"version":"1"}),
                owner,
                &Cancellation::default(),
                &bounds(),
                &mut usage,
            )
            .unwrap();
        if let Some(snapshot) = progress.snapshot {
            return snapshot;
        }
    }
    panic!("classifier did not finish");
}

#[test]
fn old_generation_first_lift_after_append_preserves_each_classifier_and_late_results() {
    let tool = format!(
        "{}\n",
        json!({"type":"assistant","uuid":"edit-event","sessionId":"s",
        "timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[
            {"type":"tool_use","id":"edit","name":"Edit","input":{"file_path":"a.rs","old_string":"old","new_string":"new"}}]}})
    );
    let result = format!(
        "{}\n",
        json!({"type":"user","uuid":"result","sessionId":"s",
        "timestamp":"2026-01-02T03:04:07Z","message":{"content":[{"type":"tool_result","tool_use_id":"edit","content":"done"}]}})
    );
    let source = RaceSource::new(&[user("a"), tool, user("b")].concat());
    let store = race_store();
    let owner = context(&store, &source, "reader");
    store
        .register_classifier(
            "merged",
            "1",
            Arc::new(|_, range| Ok(vec![false; range.len()])),
        )
        .unwrap();
    store
        .register_classifier(
            "split",
            "1",
            Arc::new(|chunks, range| {
                Ok(chunks
                    .iter()
                    .flat_map(|chunk| chunk.entries.iter())
                    .skip(range.start)
                    .take(range.len())
                    .map(|entry| matches!(entry, Entry::User(user) if user.meta.uuid != "result"))
                    .collect())
            }),
        )
        .unwrap();
    let old_response = finish(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    let old = pin(&store, &old_response, &owner);
    source.append(&[result, user("c")].concat());
    let new_response = finish(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    let new = pin(&store, &new_response, &owner);
    assert!(Arc::ptr_eq(&old.chunks[0], &new.chunks[0]));
    assert!(store.classified.lock().unwrap().is_empty());
    for classifier in ["native", "merged", "split"] {
        for (physical, expected_events, has_result) in [(&old, 3, false), (&new, 5, true)] {
            let snapshot = classified(&store, Arc::clone(physical), &owner, classifier);
            let entries = snapshot.entries();
            let flags: Vec<bool> = entries
                .iter()
                .map(|entry| {
                    classifier == "split"
                        && matches!(entry, Entry::User(user) if user.meta.uuid != "result")
                })
                .collect();
            let projected = snapshot.activity.project(
                &snapshot.session_id,
                &entries,
                &(0..snapshot.activity.turn_count()).collect::<Vec<_>>(),
            );
            let expected = crate::activity::lift_session_refs(
                &snapshot.session_id,
                &entries,
                (classifier != "native").then_some(flags.as_slice()),
            );
            assert_eq!(
                format!("{projected:?}"),
                format!("{expected:?}"),
                "{classifier}"
            );
            assert_eq!(snapshot.event_count, expected_events);
            let edit = projected
                .turns
                .iter()
                .flat_map(|turn| &turn.tool_uses)
                .next()
                .unwrap();
            assert_eq!(edit.result.is_some(), has_result);
            assert_eq!(
                snapshot.activity.turn_count(),
                if classifier == "merged" {
                    1
                } else if has_result {
                    3
                } else {
                    2
                }
            );
        }
    }
    assert_eq!(old.event_count, 3);
    assert_eq!(old.activity.turn_count(), 2);
}

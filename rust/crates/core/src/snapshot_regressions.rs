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
        max_source_read_bytes: 1024 * 1024,
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
        "deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":1000,
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
    let mut usage = [0; 20];
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

const MEMBERS: [&str; 3] = ["early", "middle", "late"];

#[derive(Debug, Default, PartialEq)]
struct Effort {
    resolutions: u64,
    builds: u64,
    lookups: u64,
    cursors: u64,
    opens: u64,
    bytes: u64,
    events: u64,
    cache_reads: u64,
    cache_bytes: u64,
}

impl Effort {
    fn since(&self, before: &Self) -> Self {
        Self {
            resolutions: self.resolutions - before.resolutions,
            builds: self.builds - before.builds,
            lookups: self.lookups - before.lookups,
            cursors: self.cursors - before.cursors,
            opens: self.opens - before.opens,
            bytes: self.bytes - before.bytes,
            events: self.events - before.events,
            cache_reads: self.cache_reads - before.cache_reads,
            cache_bytes: self.cache_bytes - before.cache_bytes,
        }
    }
}

struct Family {
    store: NativeStore,
    source: RaceSource,
    owner: Value,
    graph: Value,
    members: Vec<PathBuf>,
}

impl Family {
    fn new(store: NativeStore) -> Self {
        let source = RaceSource::new(
            &[
                user("root"),
                call("root-read", "Read", json!({"file_path":"root.rs"})),
            ]
            .concat(),
        );
        let owner = context(&store, &source, "observer");
        let members: Vec<PathBuf> = MEMBERS
            .iter()
            .map(|name| {
                let path = source.directory.join(format!("{name}.jsonl"));
                std::fs::write(&path, member(name)).unwrap();
                path
            })
            .collect();
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(root["status"].as_str(), Some("ok"), "{root:?}");
        let direct: Vec<String> = members
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let prepared = settled(
            &store,
            store.request(
                &json!({"schema":SCHEMA,"id":"observer-prepare","operation":"prepare_graph",
                "view":{"handle":root["data"]["description"]["handle"],
                "classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
                "thread_ids":[],"roots":[],"direct_paths":direct,
                "deadline_unix_ms":now_ms()+30_000,"limits":request_limits()}),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
        Self {
            graph: prepared["data"]["handle"].clone(),
            store,
            source,
            owner,
            members,
        }
    }

    fn effort(&self) -> Effort {
        let counters = self.store.state.lock().unwrap().counters;
        Effort {
            resolutions: self.store.source_resolutions.load(Ordering::Relaxed) as u64,
            builds: self.store.fact_builds.load(Ordering::Relaxed) as u64,
            lookups: self.store.fact_lookups.load(Ordering::Relaxed) as u64,
            cursors: self.store.prepared_query_records.load(Ordering::Relaxed) as u64,
            opens: counters[0],
            bytes: counters[1],
            events: counters[3],
            cache_reads: counters[18],
            cache_bytes: counters[19],
        }
    }

    fn query(&self, query: Value, observers: Option<Value>) -> Value {
        let mut request = json!({"schema":SCHEMA,"id":"observer-query","operation":"query_graph",
            "handle":self.graph,"selectors":[],"query":query,
            "deadline_unix_ms":now_ms()+30_000,"limits":request_limits()});
        if let Some(observers) = observers {
            request.insert("observers", observers);
        }
        request
    }

    fn ask(&self, query: Value, observers: Option<Value>) -> Value {
        settled(
            &self.store,
            self.store.request(
                &self.query(query, observers),
                &self.owner,
                &Cancellation::default(),
            ),
            &self.owner,
        )
    }

    fn measured(&self, query: Value, observers: Option<Value>) -> (Value, Effort) {
        let before = self.effort();
        let reply = self.ask(query, observers);
        (reply, self.effort().since(&before))
    }
}

fn call(id: &str, name: &str, input: Value) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":id,"sessionId":"s",
        "timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[
            {"type":"tool_use","id":format!("{id}-call"),"name":name,"input":input}]}})
    )
}

fn member(name: &str) -> String {
    (0..8)
        .map(|index| user(&format!("{name}-{index}")))
        .chain([
            call(
                &format!("{name}-read"),
                "Read",
                json!({"file_path":format!("{name}.rs")}),
            ),
            call(
                &format!("{name}-bash"),
                "Bash",
                json!({"command":format!("echo {name}")}),
            ),
        ])
        .collect()
}

fn reads(pattern: &str) -> Value {
    json!({"kind":"has_read","pattern":pattern,"subagents":true})
}

fn swift_store() -> NativeStore {
    NativeStore::new(
        &json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,
        "max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,
        "max_leases":16,"reserved_hook_leases":1}),
    )
    .unwrap()
}

fn request_limits() -> Value {
    json!({"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":4096,
        "max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,
        "max_sources":100})
}

fn settled(store: &NativeStore, mut response: Value, owner: &Value) -> Value {
    for _ in 0..4096 {
        let Some(cursor) = response["cursor"].as_str() else {
            return response;
        };
        response = store.request(
            &json!({"schema":SCHEMA,"id":"observer-resume","operation":"resume","cursor":cursor}),
            owner,
            &Cancellation::default(),
        );
    }
    panic!("prepared work did not settle");
}

fn observations() -> usize {
    OBSERVATIONS.with(std::cell::Cell::get)
}

#[test]
fn observers_settle_on_the_facts_a_negative_primary_already_holds() {
    let regex = json!({"kind":"has_command_regex","pattern":"echo","subagents":true});
    let alone = Family::new(swift_store());
    let shared = Family::new(swift_store());
    let (plain, spent_alone) = alone.measured(reads("absent.rs"), None);
    let unobserved = observations();
    let (observed, spent_shared) = shared.measured(
        reads("absent.rs"),
        Some(json!([
            reads("late.rs"),
            reads("nowhere.rs"),
            reads("root.rs"),
            regex.clone(),
            {"kind":"has_tool","subagents":true}
        ])),
    );
    let evaluations = observations() - unobserved;
    assert_eq!(
        plain["data"],
        json!({"kind":"scalar","value":false}),
        "{plain:?}"
    );
    assert_eq!(
        observed["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[true,false,true,null,null]}),
        "{observed:?}"
    );
    assert_eq!(evaluations, 4 + 2 * MEMBERS.len());
    assert_eq!(spent_shared, spent_alone);
    assert_eq!((spent_shared.builds, spent_shared.cursors), (3, 1));
    assert_eq!(
        shared.ask(regex, None)["data"],
        json!({"kind":"scalar","value":true})
    );
}

#[test]
fn several_negatives_reuse_the_once_loaded_child_facts() {
    let family = Family::new(swift_store());
    let negatives = || {
        Some(json!([
            reads("nowhere.rs"),
            reads("missing.rs"),
            reads("gone.rs")
        ]))
    };
    let unseen = json!({"kind":"observed_scalar","value":false,"observers":[false,false,false]});
    let (cold, loading) = family.measured(reads("absent.rs"), negatives());
    assert_eq!(cold["data"], unseen, "{cold:?}");
    assert_eq!((loading.builds, loading.cursors), (3, 1));
    let (warm, reusing) = family.measured(reads("absent.rs"), negatives());
    assert_eq!(warm["data"], unseen, "{warm:?}");
    assert_eq!(
        (
            reusing.resolutions,
            reusing.builds,
            reusing.lookups,
            reusing.cursors,
            reusing.bytes,
            reusing.cache_reads,
            reusing.cache_bytes,
        ),
        (4, 0, 0, 1, 0, 0, 0)
    );
}

#[test]
fn root_positive_primary_replies_before_any_observer_runs() {
    let family = Family::new(swift_store());
    let before = family.effort();
    let unobserved = observations();
    let reply = family.store.request(
        &family.query(
            reads("root.rs"),
            Some(json!([reads("late.rs"), reads("nowhere.rs")])),
        ),
        &family.owner,
        &Cancellation::default(),
    );
    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
    assert!(reply["cursor"].is_null(), "{reply:?}");
    assert_eq!(reply["data"], json!({"kind":"scalar","value":true}));
    assert_eq!(family.effort().since(&before), Effort::default());
    assert_eq!(observations(), unobserved);
    assert!(family
        .store
        .state
        .lock()
        .unwrap()
        .prepared_queries
        .is_empty());
}

fn rewritten(path: &Path) {
    std::fs::write(path, [member("rewritten"), user("appended")].concat()).unwrap();
}

fn truncated(path: &Path) {
    let contents = std::fs::read(path).unwrap();
    std::fs::write(path, &contents[..contents.len() / 2]).unwrap();
}

fn corrupted(family: &Family) {
    let stamp = SourceStamp::of(&std::fs::metadata(&family.members[1]).unwrap());
    let entry = family.store.prepared_disk.entry_file(
        &crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            family.owner["registry_generation"].as_str().unwrap(),
            family.owner["admission"].as_str().unwrap(),
            &family.owner["authority"],
            &json!({"id":"native","version":"1"}),
        )
        .unwrap(),
    );
    let mut bytes = std::fs::read(&entry).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&entry, bytes).unwrap();
    family
        .store
        .lock_state()
        .remove_prepared_facts(&stamp.identity)
        .unwrap();
}

fn failing_child(damage: impl Fn(&Family), status: &str, reason: &str) {
    let failure = |observers: Option<Value>| {
        let family = Family::new(swift_store());
        let validated = family.ask(reads("absent.rs"), None);
        assert_eq!(
            validated["data"],
            json!({"kind":"scalar","value":false}),
            "{validated:?}"
        );
        damage(&family);
        let failed = family.ask(reads("absent.rs"), observers);
        let lazy = family.ask(reads("root.rs"), None);
        assert_eq!(
            lazy["data"],
            json!({"kind":"scalar","value":true}),
            "{lazy:?}"
        );
        failed
    };
    let plain = failure(None);
    let observed = failure(Some(json!([reads("root.rs")])));
    assert_eq!(
        (plain["status"].as_str(), plain["reason"].as_str()),
        (Some(status), Some(reason)),
        "{plain:?}"
    );
    assert_eq!(
        (observed["status"].as_str(), observed["reason"].as_str()),
        (Some(status), Some(reason)),
        "{observed:?}"
    );
    assert!(
        plain["data"].is_null() && observed["data"].is_null(),
        "{observed:?}"
    );
}

#[test]
fn rewritten_child_fails_a_negative_primary_despite_a_root_positive_observer() {
    failing_child(
        |family| rewritten(&family.members[1]),
        "changed",
        "prepared graph source changed",
    );
}

#[test]
fn truncated_child_fails_a_negative_primary_despite_a_root_positive_observer() {
    failing_child(
        |family| truncated(&family.members[1]),
        "changed",
        "prepared graph source changed",
    );
}

#[test]
fn corrupt_child_facts_fail_a_negative_primary_despite_a_root_positive_observer() {
    failing_child(
        corrupted,
        "incomplete",
        "prepared facts revision was evicted",
    );
}

#[test]
fn primary_hit_reports_only_the_observers_it_witnessed() {
    let family = Family::new(swift_store());
    let (reply, spent) = family.measured(
        reads("middle.rs"),
        Some(json!([
            reads("early.rs"),
            reads("late.rs"),
            reads("nowhere.rs")
        ])),
    );
    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
    assert_eq!(
        reply["data"],
        json!({"kind":"observed_scalar","value":true,"observers":[true,null,null]})
    );
    assert_eq!(spent.builds, 2);
}

#[test]
fn observers_leave_every_invalidation_of_the_primary_intact() {
    let family = Family::new(swift_store());
    let altered = |key: &str, value: Value| {
        let mut owner = family.owner.clone();
        owner.insert(key, value);
        owner
    };
    let parity = |dimension: &str, graph: &Value, owner: &Value, status: &str| {
        let refusal = |observers: Option<Value>| {
            let mut request = family.query(reads("absent.rs"), observers);
            request.insert("handle", graph.clone());
            settled(
                &family.store,
                family
                    .store
                    .request(&request, owner, &Cancellation::default()),
                owner,
            )
        };
        let plain = refusal(None);
        let observed = refusal(Some(json!([
            reads("root.rs"),
            reads("late.rs"),
            reads("nowhere.rs")
        ])));
        assert_eq!(
            plain["status"].as_str(),
            Some(status),
            "{dimension}: {plain:?}"
        );
        assert_eq!(
            (observed["status"].as_str(), observed["reason"].as_str()),
            (plain["status"].as_str(), plain["reason"].as_str()),
            "{dimension}: {observed:?}"
        );
        assert!(
            plain["data"].is_null() && observed["data"].is_null(),
            "{dimension}: {observed:?}"
        );
    };
    let registry = family
        .store
        .register_tool_registry(
            &json!([{"name":"fetch","behaves_like":"Read","span_edit":null}]),
            &family.owner,
        )
        .unwrap();
    let restricted = json!({"kind":"restricted_roots",
        "effective_uid":family.owner["authority"]["effective_uid"],
        "roots":[family.source.directory.to_string_lossy().as_ref()]});
    let mut stale = family.graph.clone();
    stale.insert("revision", json!("stale-revision"));
    parity(
        "claimant",
        &family.graph,
        &altered("claimant", json!("intruder")),
        "stale_handle",
    );
    parity(
        "registry_generation",
        &family.graph,
        &altered("registry_generation", json!(registry)),
        "stale_handle",
    );
    parity(
        "admission",
        &family.graph,
        &altered("admission", json!("review")),
        "stale_handle",
    );
    parity(
        "authority",
        &family.graph,
        &altered("authority", restricted),
        "stale_handle",
    );
    parity("revision", &stale, &family.owner, "stale_handle");
    let subagents = family.source.directory.join("early/subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    std::fs::write(subagents.join("agent-new.jsonl"), user("new")).unwrap();
    parity("membership", &family.graph, &family.owner, "changed");
    let graph_id = family.graph["graph_id"].as_str().unwrap();
    family.store.state.lock().unwrap().prepared_graphs[graph_id]
        .lock()
        .unwrap()
        .classifier = json!({"id":"merged","version":"1"});
    parity("classifier", &family.graph, &family.owner, "changed");
}

fn observed_everywhere(family: &Family) -> Value {
    family.store.request(
        &family.query(
            reads("absent.rs"),
            Some(json!([
                reads("root.rs"),
                reads("early.rs"),
                reads("late.rs"),
                reads("nowhere.rs")
            ])),
        ),
        &family.owner,
        &Cancellation::default(),
    )
}

#[test]
fn parked_traversal_carries_observers_across_pages() {
    let family = Family::new(race_store());
    let mut reply = observed_everywhere(&family);
    let mut parks = 0;
    while let Some(token) = reply["cursor"].as_str().map(str::to_owned) {
        assert!(parks < 4096, "the traversal never settled: {reply:?}");
        assert!(reply["data"].is_null(), "{reply:?}");
        family.store.assert_conserved();
        {
            let state = family.store.state.lock().unwrap();
            let cursor = state
                .prepared_queries
                .get(&token)
                .expect("parked prepared query");
            assert_eq!(
                charged_bytes(&token, cursor),
                NativeStore::audit_prepared_query_bytes(&token, cursor)
            );
            if parks == 0 {
                assert_eq!(
                    (cursor.observers.witnessed, cursor.observers.pending.len()),
                    (0b0001, 3)
                );
            }
        }
        parks += 1;
        reply = resume(
            &family.store,
            &reply,
            &family.owner,
            &Cancellation::default(),
        );
    }
    assert!(parks > 1, "the traversal settled after {parks} pages");
    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
    assert_eq!(
        reply["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[true,true,true,false]})
    );
}

#[test]
fn spent_observer_allowance_leaves_unsettled_observers_unknown() {
    let family = Family::new(race_store());
    let mut reply = observed_everywhere(&family);
    let witnessed = |reply: &Value| {
        family
            .store
            .state
            .lock()
            .unwrap()
            .prepared_queries
            .get(reply["cursor"].as_str().expect("parked prepared query"))
            .expect("parked prepared query")
            .observers
            .witnessed
    };
    while witnessed(&reply) == 0b0001 {
        reply = resume(
            &family.store,
            &reply,
            &family.owner,
            &Cancellation::default(),
        );
    }
    assert_eq!(witnessed(&reply), 0b0011, "{reply:?}");
    {
        let mut state = family.store.state.lock().unwrap();
        let mut cursor = state
            .prepared_queries
            .get_mut(reply["cursor"].as_str().unwrap())
            .unwrap();
        assert_eq!(cursor.observers.pending.len(), 2);
        cursor.observers.allowance = std::time::Duration::ZERO;
    }
    let done = settled(&family.store, reply, &family.owner);
    assert_eq!(done["status"].as_str(), Some("ok"), "{done:?}");
    assert_eq!(
        done["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[true,true,null,null]})
    );
}

#[test]
fn inputs_and_unbounded_observer_lists_reply_without_observations() {
    let family = Family::new(swift_store());
    let inputs = |observers: Option<Value>| {
        let mut reply = family.store.request(
            &family.query(
                json!({"kind":"deep_predicate_inputs","order":"forward"}),
                observers,
            ),
            &family.owner,
            &Cancellation::default(),
        );
        let mut records = Vec::new();
        loop {
            assert_eq!(reply["data"]["kind"].as_str(), Some("records"), "{reply:?}");
            records.extend(
                reply["data"]["records_json"]
                    .as_array()
                    .expect("predicate input records")
                    .iter()
                    .map(|record| record.as_str().unwrap().to_owned()),
            );
            if reply["cursor"].is_null() {
                assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
                return records;
            }
            reply = resume(
                &family.store,
                &reply,
                &family.owner,
                &Cancellation::default(),
            );
        }
    };
    let plain = inputs(None);
    assert!(
        plain.iter().any(|record| record.contains("late.rs")),
        "{plain:?}"
    );
    assert_eq!(
        inputs(Some(json!([reads("root.rs"), reads("late.rs")]))),
        plain
    );
    let crowd = |count: usize| Some(json!(vec![reads("root.rs"); count]));
    assert_eq!(
        family.ask(reads("absent.rs"), crowd(33))["data"],
        json!({"kind":"scalar","value":false})
    );
    assert_eq!(
        family.ask(reads("absent.rs"), crowd(32))["data"],
        json!({"kind":"observed_scalar","value":false,"observers":vec![true; 32]})
    );
    assert_eq!(
        family.ask(reads("absent.rs"), Some(json!([])))["data"],
        json!({"kind":"scalar","value":false})
    );
}

#[test]
fn glob_and_regex_kinds_answer_only_as_the_primary_while_the_other_eight_settle_as_observers() {
    let family = Family::new(swift_store());
    let edited = json!({"kind":"has_edit_to","values":["*.zig"],"subagents":true});
    let globbed = json!({"kind":"has_read_glob","values":["*.rs"],"subagents":true});
    let matched = json!({"kind":"has_command_regex","pattern":"echo","subagents":true});
    let reply = family.ask(
        reads("absent.rs"),
        Some(json!([
            {"kind":"has_tool","pattern":"Missing","subagents":true},
            reads("nowhere.rs"),
            {"kind":"has_skill","values":["absent"],"subagents":true},
            {"kind":"has_skill_suffix","values":["absent"],"subagents":true},
            {"kind":"has_override","token":"absent-override","invalidated_by":["Edit"],"subagents":true},
            {"kind":"has_error","subagents":true},
            {"kind":"has_edit","subagents":true},
            {"kind":"has_command","values":["absent-binary"],"subagents":true},
            edited.clone(),
            globbed.clone(),
            matched.clone()
        ])),
    );
    let command = if cfg!(feature = "command") {
        json!(false)
    } else {
        json!(null)
    };
    assert_eq!(
        reply["data"],
        json!({"kind":"observed_scalar","value":false,
        "observers":[false,false,false,false,false,false,false,command,null,null,null]}),
        "{reply:?}"
    );
    assert_eq!(
        family.ask(edited, None)["data"],
        json!({"kind":"scalar","value":false})
    );
    assert_eq!(
        family.ask(globbed, None)["data"],
        json!({"kind":"scalar","value":true})
    );
    assert_eq!(
        family.ask(matched, None)["data"],
        json!({"kind":"scalar","value":true})
    );
}

#[test]
fn uncompilable_glob_observer_is_never_evaluated() {
    let glob = json!({"kind":"has_read_glob","values":["[z-a]"],"subagents":true});
    let alone = Family::new(swift_store());
    let shared = Family::new(swift_store());
    let (plain, spent_alone) = alone.measured(reads("absent.rs"), None);
    let unobserved = observations();
    let (observed, spent_shared) = shared.measured(reads("absent.rs"), Some(json!([glob])));
    let evaluations = observations() - unobserved;
    assert_eq!(plain["status"].as_str(), Some("ok"), "{plain:?}");
    assert_eq!(
        plain["data"],
        json!({"kind":"scalar","value":false}),
        "{plain:?}"
    );
    assert_eq!(
        (observed["status"].as_str(), observed["reason"].as_str()),
        (plain["status"].as_str(), plain["reason"].as_str()),
        "{observed:?}"
    );
    assert_eq!(
        observed["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[null]}),
        "{observed:?}"
    );
    assert_eq!(evaluations, 0);
    assert_eq!(spent_shared, spent_alone);
}

#[test]
fn timely_observation_evaluates_nothing_once_its_allowance_is_spent() {
    let family = Family::new(swift_store());
    let graph_id = family.graph["graph_id"].as_str().unwrap();
    let facts = Arc::clone(
        &family.store.state.lock().unwrap().prepared_graphs[graph_id]
            .lock()
            .unwrap()
            .root_facts,
    );
    let started = std::time::Instant::now();
    let unobserved = observations();
    assert_eq!(
        Observers::timely(&facts, &reads("root.rs"), started, Duration::ZERO),
        None
    );
    assert_eq!(observations(), unobserved);
    let generous = Duration::from_secs(3600);
    assert_eq!(
        Observers::timely(&facts, &reads("root.rs"), started, generous),
        Some(Some(true))
    );
    assert_eq!(
        Observers::timely(&facts, &reads("absent.rs"), started, generous),
        Some(Some(false))
    );
    assert_eq!(
        Observers::timely(
            &facts,
            &json!({"kind":"has_read","subagents":true}),
            started,
            generous
        ),
        Some(None)
    );
    assert_eq!(observations(), unobserved + 3);
}

#[test]
fn observers_that_do_not_fit_stay_unknown_and_evict_nothing() {
    let family = Family::new(swift_store());
    let observers = || {
        Some(json!([
            reads("root.rs"),
            reads("late.rs"),
            reads("nowhere.rs")
        ]))
    };
    let warmed = family.ask(reads("absent.rs"), None);
    assert_eq!(
        warmed["data"],
        json!({"kind":"scalar","value":false}),
        "{warmed:?}"
    );
    let request = family.query(reads("absent.rs"), observers());
    let base = size_of::<PreparedQueryCursor>()
        + family.owner["claimant"].as_str().unwrap().len()
        + family.graph["graph_id"].as_str().unwrap().len()
        + value_bytes(&request["query"]);
    let cached = || {
        let state = family.store.lock_state();
        (state.latest.len(), state.carried_classifications.len())
    };
    let before = cached();
    NativeStore::prune(&mut family.store.lock_state());
    let headroom =
        family.store.memory_cap(&family.owner).unwrap() - family.store.retained_accounted_bytes();
    let filler = family
        .store
        .reserve_projection(&family.owner, headroom - 2 * MAX_REPLY_BYTES - base)
        .unwrap();
    let crowded = family
        .store
        .request(&request, &family.owner, &Cancellation::default());
    drop(filler);
    assert_eq!(crowded["status"].as_str(), Some("ok"), "{crowded:?}");
    assert_eq!(
        crowded["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[true,null,null]}),
        "{crowded:?}"
    );
    assert_eq!(cached(), before);
    family.store.assert_conserved();
    assert_eq!(
        family.ask(reads("absent.rs"), observers())["data"],
        json!({"kind":"observed_scalar","value":false,"observers":[true,true,false]})
    );
}

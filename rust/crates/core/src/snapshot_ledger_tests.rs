use super::*;
use crate::snapshot_ledger::Trace;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CAP: usize = 32 * 1024 * 1024;
const HOOK_BYTES: usize = 4096;
const REPLY_RESERVATION: usize = 2 * MAX_REPLY_BYTES;
const SEARCH_LIMIT: usize = 4 * 1024 * 1024;
const SLOW_READ_STEP: usize = 64;
const WARM_HIT_BOUND: usize = 768;
const HIT_SPREAD_BOUND: usize = 4;
const STEADY_HITS: usize = 1024;
const PER_SOURCE_BOUND: usize = 16;
const PAGE_STEPS: usize = 256;
const PAGE_BOUND: usize = PER_SOURCE_BOUND * PAGE_STEPS + WARM_HIT_BOUND;
const DROP_TIMEOUT: Duration = Duration::from_secs(30);
const CHURN_TIMEOUT: Duration = Duration::from_secs(120);
const SCALING_COUNTS: [usize; 3] = [100, 500, 923];
const ENTRIES: usize = 0;
const INDEXES: usize = 1;
const PROJECTION: usize = 2;
const PENDING: usize = 3;
const TOTAL: usize = 4;
const LEASES: usize = 5;
const LOADS: usize = 6;
const GENERATIONS: usize = 7;
const PRE_SIZED_SLOTS: usize = 1792;

struct LedgerSource {
    directory: PathBuf,
    path: PathBuf,
}

impl LedgerSource {
    fn new(contents: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "cc-ledger-{}-{}-{}",
            std::process::id(),
            now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("s.jsonl");
        std::fs::write(&path, contents).unwrap();
        Self { directory, path }
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.directory.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn append(&self, contents: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
    }
}

impl Drop for LedgerSource {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}

struct Scenario {
    root: LedgerSource,
    sidechains: Vec<PathBuf>,
}

impl Scenario {
    fn new(count: usize, contents: impl Fn(usize) -> String) -> Self {
        let root = LedgerSource::new(&line("root"));
        let sidechains = (0..count)
            .map(|index| root.file(&format!("thread-{index:04}.jsonl"), &contents(index)))
            .collect();
        Self { root, sidechains }
    }

    fn ids(&self) -> Vec<String> {
        (0..self.sidechains.len())
            .map(|index| format!("thread-{index:04}"))
            .collect()
    }

    fn roots(&self) -> Vec<String> {
        vec![self.root.directory.to_string_lossy().into_owned()]
    }

    fn direct(&self) -> Vec<String> {
        self.sidechains
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

struct Fixture {
    store: NativeStore,
    owner: Value,
    request: Value,
    pins: Vec<Arc<TranscriptSnapshot>>,
}

struct Warmed {
    store: NativeStore,
    owner: Value,
    root: Value,
    graph: Value,
}

fn entry(id: &str, session: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":id,"sessionId":session,"timestamp":"2026-01-02T03:04:05Z","message":{"content":format!("prompt {id}")}})
    )
}

fn line(id: &str) -> String {
    entry(id, "s")
}

fn session_line(id: &str) -> String {
    entry(id, id)
}

fn lines(range: Range<usize>) -> String {
    range.map(|index| line(&format!("event-{index}"))).collect()
}

fn prompt_line(id: &str, bytes: usize) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":"x".repeat(bytes)}})
    )
}

fn tool_line(id: &str, tool: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":format!("{id}-call"),"name":tool,"input":{"command":"true"}}]}})
    )
}

fn deep_source(source: &LedgerSource, contents: &str) -> PathBuf {
    let directory = source
        .directory
        .join("d".repeat(200))
        .join("e".repeat(200))
        .join("f".repeat(200));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}.jsonl", "g".repeat(200)));
    std::fs::write(&path, contents).unwrap();
    path
}

fn scaling_scenario(count: usize) -> Scenario {
    Scenario::new(count, |index| {
        let id = format!("thread-{index:04}");
        if index == 3 {
            format!("{}{}", line(&id), tool_line(&format!("{id}-tool"), "Bash"))
        } else {
            line(&id)
        }
    })
}

fn context_for(claimant: &str, background: bool) -> Value {
    let mut context = json!({"claimant":claimant,"admission":"hook","authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},"registry_generation":crate::toolcall::ToolRegistrySnapshot::from_specs(HashMap::new()).fingerprint()});
    if background {
        context.insert("work_class", json!("background"));
    }
    context
}

fn cap_for(context: &Value) -> usize {
    if context["work_class"].as_str() == Some("background") {
        CAP - HOOK_BYTES
    } else {
        CAP
    }
}

fn store_with(read_step: usize, event_step: usize, extra: &[(&str, usize)]) -> NativeStore {
    let mut config = json!({"max_read_bytes_per_step":read_step,"max_events_per_step":event_step,"max_entry_bytes":64*1024,"max_retained_bytes":CAP,"reserved_hook_accounted_bytes":HOOK_BYTES,"max_leases":16,"reserved_hook_leases":1});
    for (key, value) in extra {
        config.insert(key, json!(value));
    }
    NativeStore::new(&config).unwrap()
}

fn fast_store() -> NativeStore {
    store_with(64 * 1024, 4096, &[])
}

fn slow_store() -> NativeStore {
    store_with(SLOW_READ_STEP, 2048, &[])
}

fn prepared_store() -> NativeStore {
    store_with(4096, 2048, &[])
}

fn cursor_store() -> NativeStore {
    store_with(SLOW_READ_STEP, 2, &[("max_items_per_page", 1)])
}

fn limits_json() -> Value {
    json!({"max_read_bytes":1024*1024,"max_source_read_bytes":4*1024*1024,"max_events":4096,"max_items":1024,"max_output_bytes":1024*1024,"max_discovery_entries":4096,"max_sources":1024})
}

fn work_bounds() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 1024 * 1024,
        max_source_read_bytes: 1024 * 1024,
        max_events: 4096,
        max_items: 1024,
        max_output_bytes: 1024 * 1024,
        max_discovery_entries: 4096,
        max_sources: 1024,
        deadline_unix_ms: now_ms() + 120_000,
    }
}

fn label_bounds() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 64 * 1024 * 1024,
        max_source_read_bytes: 64 * 1024 * 1024,
        max_events: 100_000,
        max_items: 100_000,
        max_output_bytes: 16 * 1024 * 1024,
        max_discovery_entries: 4096,
        max_sources: 1024,
        deadline_unix_ms: now_ms() + 120_000,
    }
}

fn acquire(path: &Path) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-acquire","operation":"acquire","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn classifier_acquire(path: &Path, classifier: &str, max_read_bytes: usize) -> Value {
    let mut request = acquire(path);
    request.insert("classifier", json!({"id":classifier,"version":"1"}));
    request["limits"].insert("max_read_bytes", json!(max_read_bytes));
    request
}

fn prepare_request(
    root: &Value,
    thread_ids: &[String],
    roots: &[String],
    direct: &[String],
) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-prepare","operation":"prepare_graph","view":{"handle":root,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":thread_ids,"roots":roots,"direct_paths":direct,"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn graph_query(graph: &Value, query: Value, selectors: Value) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-query","operation":"query_graph","handle":graph,"selectors":selectors,"query":query,"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn missing_tool() -> Value {
    json!({"kind":"has_tool","pattern":"Missing","subagents":true})
}

fn marker_tool() -> Value {
    json!({"kind":"has_tool","pattern":"Bash","subagents":true})
}

fn warm_request(scenario: &Scenario) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":scenario.ids(),"roots":scenario.roots(),"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn resume_request(cursor: &str) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-resume","operation":"resume","cursor":cursor})
}

fn resume(store: &NativeStore, response: &Value, owner: &Value) -> Value {
    let cursor = response["cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("no continuation: {response:?}"));
    store.request(&resume_request(cursor), owner, &Cancellation::default())
}

fn settle(store: &NativeStore, mut response: Value, owner: &Value) -> Value {
    for _ in 0..4096 {
        if response["cursor"].as_str().is_none() {
            return response;
        }
        response = resume(store, &response, owner);
    }
    panic!("continuation never settled: {response:?}");
}

fn drive(store: &NativeStore, mut response: Value, owner: &Value) -> Value {
    for _ in 0..4096 {
        store.assert_conserved();
        if response["cursor"].as_str().is_none() {
            return response;
        }
        response = resume(store, &response, owner);
    }
    panic!("continuation never settled: {response:?}");
}

fn parked_until_settled(store: &NativeStore, request: &Value, owner: &Value) -> Value {
    let first = store.request(request, owner, &Cancellation::default());
    assert!(
        first["cursor"].as_str().is_some(),
        "the request never parked a cursor: {first:?}"
    );
    drive(store, first, owner)
}

fn ok_handle(response: &Value) -> Value {
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    response["data"]["description"]["handle"].clone()
}

fn acquired(store: &NativeStore, path: &Path, owner: &Value) -> (Value, Arc<TranscriptSnapshot>) {
    let handle = ok_handle(&drive(
        store,
        store.request(&acquire(path), owner, &Cancellation::default()),
        owner,
    ));
    let snapshot = store.pin(&handle, owner).unwrap();
    (handle, snapshot)
}

fn classified(
    store: &NativeStore,
    path: &Path,
    classifier: &str,
    owner: &Value,
) -> (Value, Arc<TranscriptSnapshot>) {
    let handle = ok_handle(&drive(
        store,
        store.request(
            &classifier_acquire(path, classifier, 1024 * 1024),
            owner,
            &Cancellation::default(),
        ),
        owner,
    ));
    let snapshot = store.pin(&handle, owner).unwrap();
    (handle, snapshot)
}

fn prepared_graph(store: &NativeStore, root: &Value, direct: &[String], owner: &Value) -> Value {
    let prepared = drive(
        store,
        store.request(
            &prepare_request(root, &[], &[], direct),
            owner,
            &Cancellation::default(),
        ),
        owner,
    );
    assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
    prepared["data"]["handle"].clone()
}

fn warm(store: &NativeStore, scenario: &Scenario, owner: &Value) {
    let mut background = owner.clone();
    background.insert("work_class", json!("background"));
    let mut request = warm_request(scenario);
    for _ in 0..scenario.sidechains.len() + 16 {
        let reply = store.request(&request, &background, &Cancellation::default());
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        if reply["data"]["complete"].as_bool() == Some(true) {
            return;
        }
        request.insert("start_index", reply["data"]["next_index"].clone());
        request.insert(
            "membership_revision",
            reply["data"]["membership_revision"].clone(),
        );
    }
    panic!("registered warming never completed");
}

fn warmed_registry(store: NativeStore, scenario: &Scenario) -> Warmed {
    let owner = context_for("scaling", false);
    let root = settle(
        &store,
        store.request(
            &acquire(&scenario.root.path),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    let root = ok_handle(&root);
    warm(&store, scenario, &owner);
    let prepared = settle(
        &store,
        store.request(
            &prepare_request(&root, &scenario.ids(), &scenario.roots(), &[]),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
    let graph = prepared["data"]["handle"].clone();
    let negative = settle(
        &store,
        store.request(
            &graph_query(&graph, missing_tool(), json!([])),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    assert_eq!(negative["status"].as_str(), Some("ok"), "{negative:?}");
    assert_eq!(negative["data"]["value"].as_bool(), Some(false));
    Warmed {
        store,
        owner,
        root,
        graph,
    }
}

fn warm_hit(store: &NativeStore, path: &Path, owner: &Value) {
    let response = settle(
        store,
        store.request(&acquire(path), owner, &Cancellation::default()),
        owner,
    );
    release_lease(store, &ok_handle(&response), owner);
}

fn stats_gauges(store: &NativeStore, owner: &Value) -> Value {
    let stats = store.request(
        &json!({"schema":SCHEMA,"id":"ledger-stats","operation":"stats"}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(stats["status"].as_str(), Some("ok"), "{stats:?}");
    stats["data"]["gauges"].clone()
}

fn release_lease(store: &NativeStore, handle: &Value, owner: &Value) {
    let released = store.request(
        &json!({"schema":SCHEMA,"id":"ledger-release","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle["lease_id"]}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
    assert_eq!(
        released["data"]["released"].as_bool(),
        Some(true),
        "{released:?}"
    );
}

fn release_cursor(store: &NativeStore, cursor: &str, owner: &Value) {
    let released = store.request(
        &json!({"schema":SCHEMA,"id":"ledger-release","operation":"release","kind":"cursor","token":cursor}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
    assert_eq!(
        released["data"]["released"].as_bool(),
        Some(true),
        "{released:?}"
    );
}

fn release_graph(store: &NativeStore, graph: &Value, owner: &Value) {
    let released = store.request(
        &json!({"schema":SCHEMA,"id":"ledger-release","operation":"release","kind":"graph","token":graph["graph_id"],"owner_epoch":graph["owner_epoch"]}),
        owner,
        &Cancellation::default(),
    );
    assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
    assert_eq!(
        released["data"]["released"].as_bool(),
        Some(true),
        "{released:?}"
    );
}

fn evict_unpinned(store: &NativeStore, owner: &Value) {
    let refused = store.reserve_projection(owner, 2 * CAP);
    assert!(matches!(refused, Err(error) if error.status == Status::RetainedLimit));
}

fn orphaned(
    store: &NativeStore,
    source: &LedgerSource,
    owner: &Value,
) -> (Arc<TranscriptSnapshot>, Arc<TranscriptSnapshot>) {
    let (handle, orphan) = acquired(store, &source.path, owner);
    assert_eq!(orphan.chunks.len(), 1);
    release_lease(store, &handle, owner);
    std::fs::write(&source.path, lines(0..3)).unwrap();
    let (_, replacement) = acquired(store, &source.path, owner);
    assert!(!Arc::ptr_eq(&orphan.chunks[0], &replacement.chunks[0]));
    assert_eq!(settled(store)[GENERATIONS], 2);
    assert_eq!(Arc::strong_count(&orphan), 1);
    (orphan, replacement)
}

fn submit_labels(store: &NativeStore, page: &Value, owner: &Value) -> Value {
    let records = page["records_json"].as_array().unwrap().len();
    let labels: Vec<bool> = (0..records).map(|index| index % 2 == 0).collect();
    store
        .submit_classifier(
            page["cursor"].as_str().unwrap(),
            &labels,
            owner,
            &Cancellation::default(),
        )
        .unwrap()
}

fn recording_classifier(store: &NativeStore, id: &str) -> Arc<Mutex<Vec<Range<usize>>>> {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&batches);
    store
        .register_classifier(
            id,
            "1",
            Arc::new(move |_, range| {
                recorded.lock().unwrap().push(range.clone());
                Ok(range.map(|position| position % 2 == 0).collect())
            }),
        )
        .unwrap();
    batches
}

fn ledger(store: &NativeStore) -> [usize; 8] {
    let mut state = store.lock_state();
    let gauges = NativeStore::gauges(&mut state);
    NativeStore::GAUGE_KEYS.map(|key| number(&gauges, key).unwrap())
}

fn audited(store: &NativeStore) -> [usize; 8] {
    let state = store.lock_state();
    let gauges = NativeStore::audit_gauges(&state);
    NativeStore::GAUGE_KEYS.map(|key| number(&gauges, key).unwrap())
}

fn settled(store: &NativeStore) -> [usize; 8] {
    NativeStore::prune(&mut store.lock_state());
    ledger(store)
}

fn settled_charges(store: &NativeStore) -> usize {
    let mut state = store.lock_state();
    NativeStore::prune(&mut state);
    number(
        &NativeStore::gauges(&mut state),
        "retained_total_accounted_bytes",
    )
    .unwrap()
        - NativeStore::fixed_metadata_bytes(&state)
}

fn warm_buffer_charge(store: &NativeStore) -> usize {
    store.lock_state().ledger.shared.warm()
}

fn bookkeeping(store: &NativeStore) -> usize {
    NativeStore::fixed_metadata_bytes(&store.lock_state())
}

fn traced(store: &NativeStore) -> Vec<Trace> {
    store.lock_state().ledger.shared.work().traced()
}

fn lease_table(store: &NativeStore) -> (usize, usize, usize, usize) {
    let state = store.lock_state();
    (
        state.leases.len(),
        state.leases.reserved(),
        state.leases.charged(),
        state.leases.capacity_bytes(),
    )
}

fn location_heap(store: &NativeStore) -> (usize, usize, usize) {
    let state = store.lock_state();
    (
        state.locations_expiry.len(),
        state.locations_expiry.reserved(),
        state.locations_expiry.heap_bytes(),
    )
}

fn publication_tables(store: &NativeStore) -> (usize, usize) {
    let state = store.lock_state();
    (state.generations.reserved(), state.ledger.shared.reserved())
}

fn issued(fixture: &Fixture) -> bool {
    let mut state = fixture.store.lock_state();
    match fixture.store.issue(
        &mut state,
        Arc::clone(&fixture.pins[0]),
        fixture.request.clone(),
        &fixture.owner,
        now_ms() + 120_000,
        fixture.store.token("lease"),
        0,
    ) {
        Ok(data) => {
            assert_eq!(data["kind"].as_str(), Some("acquired"), "{data:?}");
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("lease issue failed outside admission: {error:?}"),
    }
}

fn remembered(fixture: &Fixture) -> bool {
    let id = fixture.request["id"].as_str().unwrap().to_owned();
    let path = PathBuf::from(fixture.request["path"].as_str().unwrap());
    fixture
        .store
        .remember_locations(&[(id.clone(), path)], &fixture.owner);
    fixture.store.lock_state().locations.contains_key(&id)
}

fn location_bytes(fixture: &Fixture) -> usize {
    fixture.request["id"].as_str().unwrap().len()
        + fixture.request["path"].as_str().unwrap().len()
        + size_of::<LocatedPath>()
}

fn lease_fixture(source: &LedgerSource, background: bool, full: bool) -> Fixture {
    let store = fast_store();
    let owner = context_for("lease", background);
    let (_, snapshot) = acquired(&store, &source.path, &owner);
    let fixture = Fixture {
        store,
        owner,
        request: json!({"id":"native","version":"1"}),
        pins: vec![snapshot],
    };
    while full && {
        let (len, reserved, _, _) = lease_table(&fixture.store);
        len < reserved
    } {
        assert!(issued(&fixture), "lease table padding refused");
    }
    let (len, reserved, _, _) = lease_table(&fixture.store);
    assert_eq!(len == reserved, full, "lease table padding missed");
    fixture
}

fn location_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = fast_store();
    let owner = context_for("locate", background);
    let path = source.path.to_string_lossy().into_owned();
    for index in 0.. {
        let (len, reserved, _) = location_heap(&store);
        if len == reserved && len > 0 {
            break;
        }
        store.remember_locations(
            &[(format!("session-{index:04}"), source.path.clone())],
            &owner,
        );
    }
    {
        let state = store.lock_state();
        assert!(
            state.locations.len() < state.locations.reserved(),
            "the location table would grow with the heap"
        );
    }
    Fixture {
        store,
        owner,
        request: json!({"id":"session-next","path":path}),
        pins: Vec::new(),
    }
}

fn padded_classification(
    source: &LedgerSource,
    extras: &[LedgerSource],
    classifier: &str,
    background: bool,
) -> Fixture {
    let mut fixture = interrupted_classification(source, classifier, background);
    for extra in extras {
        let full = {
            let state = fixture.store.lock_state();
            state.generations.len() == state.generations.reserved()
        };
        if full {
            break;
        }
        fixture
            .pins
            .push(acquired(&fixture.store, &extra.path, &fixture.owner).1);
    }
    let mut state = fixture.store.lock_state();
    assert_eq!(
        state.generations.len(),
        state.generations.reserved(),
        "generation table padding missed"
    );
    for id in 1.. {
        if state.ledger.shared.len() == state.ledger.shared.reserved() {
            break;
        }
        state.ledger.shared.acquire(Anchor::facts(id, 0));
    }
    drop(state);
    fixture
}

fn chunk_charge(chunk: &EntryChunk) -> usize {
    chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes
}

fn entry_bytes(snapshots: &[&Arc<TranscriptSnapshot>]) -> usize {
    let mut seen = HashSet::new();
    snapshots
        .iter()
        .flat_map(|snapshot| snapshot.accounted_allocations())
        .filter(|(id, _)| seen.insert(*id))
        .map(|(_, charge)| charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
        .sum()
}

#[derive(Debug, Clone, Copy)]
struct Measured {
    work: usize,
    copies: usize,
    reclaims: usize,
}

fn measured(store: &NativeStore, operation: impl FnOnce()) -> Measured {
    let work = store.retained_work.load(Ordering::Relaxed);
    let copies = store.warm_copies.load(Ordering::Relaxed);
    let reclaims = store.reclaims.load(Ordering::Relaxed);
    operation();
    Measured {
        work: store.retained_work.load(Ordering::Relaxed) - work,
        copies: store.warm_copies.load(Ordering::Relaxed) - copies,
        reclaims: store.reclaims.load(Ordering::Relaxed) - reclaims,
    }
}

fn cached_hit(store: &NativeStore, path: &Path, owner: &Value, label: &str) -> Measured {
    measured(store, || {
        let response = store.request(&acquire(path), owner, &Cancellation::default());
        assert_eq!(
            response["status"].as_str(),
            Some("ok"),
            "{label}: {response:?}"
        );
        assert!(
            response["cursor"].is_null(),
            "{label} was not cached: {response:?}"
        );
        let handle = &response["data"]["description"]["handle"];
        store.validate_scope(handle, owner).unwrap();
        release_lease(store, handle, owner);
    })
}

fn assert_steady(label: &str, work: &[usize]) {
    let (min, max) = (
        work.iter().copied().min().unwrap(),
        work.iter().copied().max().unwrap(),
    );
    assert!(
        max - min <= HIT_SPREAD_BOUND,
        "{label}: retained work drifted from {min} to {max}: {work:?}"
    );
}

fn fill_to<'a>(
    store: &'a NativeStore,
    owner: &Value,
    headroom: usize,
) -> ProjectionReservation<'a> {
    NativeStore::prune(&mut store.lock_state());
    let cap = cap_for(owner);
    let used = ledger(store)[TOTAL];
    assert!(
        used + headroom <= cap,
        "headroom {headroom} does not fit above {used} accounted bytes"
    );
    let filler = store
        .reserve_projection(owner, cap - used - headroom)
        .unwrap();
    assert_eq!(ledger(store)[TOTAL], cap - headroom);
    filler
}

fn paged_costs(store: &NativeStore, request: Value, owner: &Value) -> (Value, Vec<Measured>) {
    let mut next = request;
    let mut costs = Vec::new();
    loop {
        let mut reply = None;
        costs.push(measured(store, || {
            reply = Some(store.request(&next, owner, &Cancellation::default()));
        }));
        let reply = reply.unwrap();
        match reply["cursor"].as_str() {
            Some(cursor) => next = resume_request(cursor),
            None => return (reply, costs),
        }
    }
}

fn assert_page_costs(pass: &str, count: usize, pages: &[Measured]) {
    for (page, Measured { work, copies, .. }) in pages.iter().copied().enumerate() {
        assert!(
            work <= PAGE_BOUND,
            "{pass} over {count} sources did {work} units of retained work on page request {page}"
        );
        assert!(
            copies <= WARM_HIT_BOUND,
            "{pass} over {count} sources copied {copies} retained elements on page request {page}"
        );
    }
}

fn submit(fixture: &Fixture) -> Value {
    fixture
        .store
        .request(&fixture.request, &fixture.owner, &Cancellation::default())
}

fn admitted(response: &Value, expected: &str) -> bool {
    match response["status"].as_str() {
        Some("retained_limit") => false,
        Some(status) if status == expected => true,
        _ => panic!("expected {expected} or retained_limit: {response:?}"),
    }
}

fn parked(response: &Value) -> bool {
    let accepted = admitted(response, "incomplete");
    if accepted {
        assert!(response["cursor"].as_str().is_some(), "{response:?}");
    }
    accepted
}

fn exact_headroom(build: &dyn Fn() -> Fixture, attempt: &dyn Fn(&Fixture) -> bool) -> usize {
    let probe = |headroom: usize| {
        let fixture = build();
        let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
        attempt(&fixture)
    };
    assert!(
        probe(SEARCH_LIMIT),
        "nothing was admitted within {SEARCH_LIMIT} bytes of headroom"
    );
    if probe(0) {
        return 0;
    }
    let (mut refused, mut fits) = (0, SEARCH_LIMIT);
    while fits - refused > 1 {
        let middle = refused + (fits - refused) / 2;
        if probe(middle) {
            fits = middle;
        } else {
            refused = middle;
        }
    }
    fits
}

fn assert_boundary_at(
    site: &str,
    fixture: &Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    exact: usize,
) {
    assert_refused_at(site, fixture, attempt, exact);
    assert_fitted_at(site, fixture, attempt, exact);
}

fn assert_refused_at(
    site: &str,
    fixture: &Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    exact: usize,
) {
    let cap = cap_for(&fixture.owner);
    let _filler = fill_to(&fixture.store, &fixture.owner, exact - 1);
    fixture.store.assert_conserved();
    let before = (
        ledger(&fixture.store),
        audited(&fixture.store),
        bookkeeping(&fixture.store),
    );
    traced(&fixture.store);
    assert!(
        !attempt(fixture),
        "{site}: one byte over the exact fit was admitted"
    );
    fixture.store.assert_conserved();
    assert_eq!(
        (
            ledger(&fixture.store),
            audited(&fixture.store),
            bookkeeping(&fixture.store),
        ),
        before,
        "{site}: the refused admission leaked state"
    );
    assert!(
        !traced(&fixture.store)
            .iter()
            .any(|trace| matches!(trace, Trace::Allocated(_))),
        "{site}: the refused admission allocated retained bookkeeping"
    );
    assert!(
        audited(&fixture.store)[TOTAL] <= cap,
        "{site}: the refusal left the audit above the cap"
    );
}

fn assert_fitted_at(
    site: &str,
    fixture: &Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    exact: usize,
) {
    let cap = cap_for(&fixture.owner);
    let _filler = fill_to(&fixture.store, &fixture.owner, exact);
    assert!(attempt(fixture), "{site}: the exact fit was refused");
    fixture.store.assert_conserved();
    assert!(
        audited(&fixture.store)[TOTAL] <= cap,
        "{site}: the admitted publication exceeded the cap"
    );
}

fn assert_exact_growth(
    site: &str,
    fixture: &Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    exact: usize,
    charged: &dyn Fn(&NativeStore) -> usize,
) -> usize {
    let cap = cap_for(&fixture.owner);
    let _filler = fill_to(&fixture.store, &fixture.owner, exact);
    let (charge, growth) = (charged(&fixture.store), bookkeeping(&fixture.store));
    assert!(attempt(fixture), "{site}: the exact fit was refused");
    fixture.store.assert_conserved();
    let (charge, growth) = (
        charged(&fixture.store) - charge,
        bookkeeping(&fixture.store) - growth,
    );
    eprintln!("{site}: charge={charge} growth={growth} headroom={exact}");
    assert_eq!(
        exact,
        charge + growth,
        "{site}: the exact headroom is not the charge plus its bookkeeping growth"
    );
    assert_eq!(
        ledger(&fixture.store)[TOTAL],
        cap,
        "{site}: the exact fit did not land on the cap"
    );
    assert_eq!(audited(&fixture.store)[TOTAL], cap);
    growth
}

fn assert_admitted_before_allocating(site: &str, traced: &[Trace]) {
    let admitted = traced
        .iter()
        .position(|trace| matches!(trace, Trace::Admitted(_)))
        .unwrap_or_else(|| panic!("{site}: nothing was admitted: {traced:?}"));
    let allocated: Vec<_> = traced
        .iter()
        .enumerate()
        .filter(|(_, trace)| matches!(trace, Trace::Allocated(_)))
        .map(|(at, _)| at)
        .collect();
    assert!(
        !allocated.is_empty(),
        "{site}: the full table did not grow: {traced:?}"
    );
    assert!(
        allocated.iter().all(|at| *at > admitted),
        "{site}: retained bookkeeping grew before its admission: {traced:?}"
    );
}

fn assert_admission_boundary(
    site: &str,
    build: &dyn Fn() -> Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    floor: usize,
) {
    let exact = exact_headroom(build, attempt);
    assert!(
        exact > floor,
        "{site}: the binding admission needs {exact} bytes, not past the {floor}-byte floor"
    );
    assert_boundary_at(site, &build(), attempt, exact);
}

fn registered_graph_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("registered", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    warm(&store, scenario, &owner);
    let request = prepare_request(&root, &scenario.ids(), &scenario.roots(), &[]);
    let first = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
    let mut pins = vec![root_snapshot];
    pins.extend(
        scenario
            .sidechains
            .iter()
            .map(|path| acquired(&store, path, &owner).1),
    );
    Fixture {
        store,
        owner,
        request,
        pins,
    }
}

fn direct_graph_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("direct", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let request = prepare_request(&root, &[], &[], &scenario.direct());
    let first = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
    Fixture {
        store,
        owner,
        request,
        pins: vec![root_snapshot],
    }
}

fn input_page_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let store = prepared_store();
    let owner = context_for("inputs", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    let mut request = graph_query(
        &graph,
        json!({"kind":"deep_predicate_inputs","order":"forward","padding":"p".repeat(padding)}),
        json!([]),
    );
    request["limits"].insert("max_items", json!(1));
    Fixture {
        store,
        owner,
        request,
        pins: vec![root_snapshot],
    }
}

fn slice_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("slices", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &[], &owner);
    let request = graph_query(
        &graph,
        json!({"kind":"has_read","pattern":"missing","subagents":true}),
        json!([{"kind":"current_turn"}]),
    );
    Fixture {
        store,
        owner,
        request,
        pins: vec![root_snapshot],
    }
}

fn interrupted_classification(
    source: &LedgerSource,
    classifier: &str,
    background: bool,
) -> Fixture {
    let store = fast_store();
    let owner = context_for("classify", background);
    let (_, native) = acquired(&store, &source.path, &owner);
    let interrupt = Cancellation::default();
    let trigger = interrupt.clone();
    store
        .register_classifier(
            classifier,
            "1",
            Arc::new(move |_, range| {
                trigger.cancel();
                Ok(vec![false; range.len()])
            }),
        )
        .unwrap();
    let request = json!({"id":classifier,"version":"1"});
    let interrupted = store.classify(
        Arc::clone(&native),
        &request,
        &owner,
        &interrupt,
        &work_bounds(),
        &mut [0u64; 18],
    );
    assert!(matches!(interrupted, Err(error) if error.status == Status::Cancelled));
    Fixture {
        store,
        owner,
        request,
        pins: vec![native],
    }
}

fn classification_published(fixture: &Fixture) -> bool {
    match fixture.store.classify(
        Arc::clone(&fixture.pins[0]),
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &work_bounds(),
        &mut [0u64; 18],
    ) {
        Ok(progress) => {
            let derived = progress.snapshot.expect("published classification");
            assert_eq!(derived.event_count, 2);
            assert_eq!(derived.activity.entry_count(), 2);
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("classification failed outside admission: {error:?}"),
    }
}

fn classifier_stage_fixture(source: &LedgerSource, classifier: &str, background: bool) -> Fixture {
    let store = fast_store();
    let owner = context_for("classify", background);
    let (_, native) = acquired(&store, &source.path, &owner);
    store
        .register_classifier(
            classifier,
            "1",
            Arc::new(|_, range| Ok(vec![false; range.len()])),
        )
        .unwrap();
    Fixture {
        store,
        owner,
        request: json!({"id":classifier,"version":"1"}),
        pins: vec![native],
    }
}

fn stage_created(fixture: &Fixture) -> bool {
    let bounds = WorkLimits {
        max_events: 0,
        ..work_bounds()
    };
    match fixture.store.classify(
        Arc::clone(&fixture.pins[0]),
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &bounds,
        &mut [0u64; 18],
    ) {
        Ok(_) => panic!("classifier stage creation progressed past its event budget"),
        Err(error) if error.status == Status::Incomplete => true,
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("classifier stage creation failed outside admission: {error:?}"),
    }
}

fn publishing_request(path: &Path, background: bool) -> usize {
    let store = slow_store();
    let owner = context_for("advance", background);
    let mut response = store.request(&acquire(path), &owner, &Cancellation::default());
    let mut requests = 1;
    while response["status"].as_str() == Some("incomplete") {
        response = resume(&store, &response, &owner);
        requests += 1;
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(requests > 2, "the load published in {requests} requests");
    requests
}

fn parked_load(path: &Path, background: bool, requests: usize) -> Fixture {
    let store = slow_store();
    let owner = context_for("advance", background);
    let mut request = store.request(&acquire(path), &owner, &Cancellation::default());
    for _ in 2..requests {
        request = resume(&store, &request, &owner);
    }
    assert_eq!(
        request["status"].as_str(),
        Some("incomplete"),
        "{request:?}"
    );
    Fixture {
        store,
        owner,
        request,
        pins: Vec::new(),
    }
}

fn advance_published(fixture: &Fixture) -> bool {
    let token = fixture.request["cursor"].as_str().unwrap();
    let waiter = fixture
        .store
        .lock_state()
        .waiters
        .get(token)
        .cloned()
        .expect("parked reservation");
    match fixture.store.advance(
        token,
        waiter,
        None,
        &Cancellation::default(),
        &mut [0u64; 18],
    ) {
        Ok((data, cursor, reason)) => {
            assert!(cursor.is_none() && reason.is_none(), "{data:?}");
            assert_eq!(data["kind"].as_str(), Some("acquired"), "{data:?}");
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("advance failed outside admission: {error:?}"),
    }
}

#[test]
fn anchor_table_charge_moves_only_when_the_table_reallocates() {
    let work = Work::default();
    let mut ledger = RetainedLedger::new(work.clone());
    let anchors = &mut ledger.shared;
    let mut next = 1usize;
    let mut live = Vec::new();
    while anchors.len() < 14 {
        anchors.acquire(Anchor::facts(next, 0));
        live.push(next);
        next += 1;
    }
    assert_eq!(anchors.reserved(), 14);
    assert_eq!(anchors.capacity(), 14);
    let charged = anchors.table_bytes();
    work.traced();
    let mut dipped = false;
    let mut rounds = 0;
    while !dipped && rounds < 256 {
        anchors.release(live.remove(0));
        anchors.audit_reserved();
        dipped |= anchors.capacity() < anchors.reserved();
        assert_eq!(anchors.table_bytes(), charged, "release moved the charge");
        anchors.acquire(Anchor::facts(next, 0));
        live.push(next);
        next += 1;
        anchors.audit_reserved();
        let reallocated = work.traced();
        if anchors.table_bytes() != charged {
            assert_eq!(reallocated, vec![Trace::Allocated(anchors.reserved())]);
            break;
        }
        assert!(reallocated.is_empty(), "allocation without a charge change");
        rounds += 1;
    }
    assert!(dipped, "no erase left a tombstone in {rounds} rounds");
    let charged = anchors.table_bytes();
    let reserved = anchors.reserved();
    work.traced();
    while anchors.len() < reserved {
        anchors.acquire(Anchor::facts(next, 0));
        next += 1;
        assert_eq!(anchors.table_bytes(), charged);
    }
    assert!(work.traced().is_empty());
    anchors.acquire(Anchor::facts(next, 0));
    assert!(anchors.reserved() > reserved);
    assert_eq!(work.traced(), vec![Trace::Allocated(anchors.reserved())]);
    assert_eq!(
        anchors.table_bytes(),
        anchors.reserved() * (charged / reserved)
    );
    anchors.audit_reserved();
}

#[test]
fn shared_chunk_prefix_stays_charged_until_its_last_generation_dies() {
    let source = LedgerSource::new(&lines(0..2));
    let store = fast_store();
    let owner = context_for("prefix", false);
    let (first_handle, first) = acquired(&store, &source.path, &owner);
    source.append(&lines(2..4));
    let (second_handle, second) = acquired(&store, &source.path, &owner);
    assert!(Arc::ptr_eq(&first.chunks[0], &second.chunks[0]));
    store.assert_conserved();
    assert_eq!(ledger(&store)[ENTRIES], entry_bytes(&[&first, &second]));
    release_lease(&store, &first_handle, &owner);
    drop(first);
    store.assert_conserved();
    let survivor = settled(&store);
    assert_eq!(survivor[GENERATIONS], 1);
    assert_eq!(survivor[ENTRIES], entry_bytes(&[&second]));
    release_lease(&store, &second_handle, &owner);
    evict_unpinned(&store, &owner);
    store.assert_conserved();
    assert_eq!(ledger(&store)[GENERATIONS], 1);
    drop(second);
    store.assert_conserved();
    assert_eq!(ledger(&store)[GENERATIONS], 1);
    evict_unpinned(&store, &owner);
    store.assert_conserved();
    let released = ledger(&store);
    assert_eq!(released[GENERATIONS], 0);
    assert_eq!(released[ENTRIES], 0);
}

#[test]
fn escaped_chunk_rows_stay_charged_until_their_last_owner_drops() {
    let source = LedgerSource::new(&line("escaped"));
    let store = fast_store();
    let owner = context_for("escaped", false);
    let (handle, snapshot) = acquired(&store, &source.path, &owner);
    assert_eq!(snapshot.chunks.len(), 1);
    let charge = chunk_charge(&snapshot.chunks[0]);
    let rows = Arc::clone(&snapshot.chunks[0].entries);
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0], Entry::User(user) if user.meta.uuid == "escaped"));
    release_lease(&store, &handle, &owner);
    drop(snapshot);
    evict_unpinned(&store, &owner);
    store.assert_conserved();
    assert_eq!(Arc::strong_count(&rows), 1);
    let escaped = ledger(&store);
    assert_eq!(escaped[GENERATIONS], 0);
    assert_eq!(escaped[ENTRIES], charge);
    drop(rows);
    store.assert_conserved();
    assert_eq!(ledger(&store)[ENTRIES], 0);
}

#[test]
fn observations_under_one_guard_drain_deaths_after_the_lock() {
    let source = LedgerSource::new(&line("guarded"));
    let store = fast_store();
    let owner = context_for("guarded", false);
    let (orphan, replacement) = orphaned(&store, &source, &owner);
    let charge = chunk_charge(&orphan.chunks[0]);
    let rows = Arc::clone(&orphan.chunks[0].entries);
    let mut state = store.lock_state();
    drop(orphan);
    let gauges = NativeStore::gauges(&mut state);
    assert_eq!(number(&gauges, "live_generations").unwrap(), 1);
    assert_eq!(
        number(&gauges, "retained_entry_capacity_bytes").unwrap(),
        entry_bytes(&[&replacement]) + charge
    );
    drop(rows);
    NativeStore::prune(&mut state);
    let audit = NativeStore::audit_gauges(&state);
    let observed = NativeStore::gauges(&mut state);
    for key in NativeStore::GAUGE_KEYS {
        assert_eq!(
            number(&observed, key).unwrap(),
            number(&audit, key).unwrap(),
            "{key}"
        );
    }
    assert_eq!(
        number(&observed, "retained_entry_capacity_bytes").unwrap(),
        entry_bytes(&[&replacement])
    );
    drop(state);
    store.assert_conserved();
}

#[test]
fn stats_reports_the_ledger_plus_its_reply_reservation() {
    let source = LedgerSource::new(&lines(0..3));
    let store = fast_store();
    let owner = context_for("stats", false);
    let _retained = acquired(&store, &source.path, &owner);
    let before = settled(&store);
    let gauges = stats_gauges(&store, &owner);
    assert_eq!(
        gauges.as_object().unwrap().len(),
        NativeStore::GAUGE_KEYS.len()
    );
    let mut expected = before;
    expected[PENDING] += REPLY_RESERVATION;
    expected[TOTAL] += REPLY_RESERVATION;
    assert_eq!(
        NativeStore::GAUGE_KEYS.map(|key| number(&gauges, key).unwrap()),
        expected
    );
    assert_eq!(ledger(&store), before);
    store.assert_conserved();
}

#[test]
fn completed_classifier_stages_prune_without_unbalancing_the_ledger() {
    let source = LedgerSource::new(&lines(0..3));
    let store = fast_store();
    let owner = context_for("stages", false);
    let batches = recording_classifier(&store, "complete");
    let mut expected = Vec::new();
    for round in 0..3 {
        if round > 0 {
            source.append(&lines(3 * round..3 * round + 3));
        }
        let (handle, snapshot) = classified(&store, &source.path, "complete", &owner);
        expected.push(3 * round..3 * round + 3);
        assert_eq!(snapshot.event_count, 3 * round + 3);
        assert_eq!(*batches.lock().unwrap(), expected);
        store.assert_conserved();
        NativeStore::prune(&mut store.lock_state());
        store.assert_conserved();
        let pruned = ledger(&store);
        assert_eq!(pruned[PENDING], 0);
        assert_eq!(pruned[LOADS], 0);
        release_lease(&store, &handle, &owner);
        drop(snapshot);
        store.assert_conserved();
    }
}

#[test]
fn custom_classifier_carries_reinserts_and_evicts_its_classification() {
    let source = LedgerSource::new(&lines(0..3));
    let store = fast_store();
    let owner = context_for("carried", false);
    let batches = recording_classifier(&store, "carried");
    let mut retained = vec![classified(&store, &source.path, "carried", &owner)];
    let mut expected = vec![0..3];
    store.assert_conserved();
    for round in 0..6 {
        let start = 3 + 2 * round;
        source.append(&lines(start..start + 2));
        retained.push(classified(&store, &source.path, "carried", &owner));
        expected.push(start..start + 2);
        assert_eq!(*batches.lock().unwrap(), expected, "round {round}");
        store.assert_conserved();
    }
    for (handle, snapshot) in retained {
        release_lease(&store, &handle, &owner);
        drop(snapshot);
        store.assert_conserved();
    }
    evict_unpinned(&store, &owner);
    store.assert_conserved();
    assert_eq!(settled(&store)[GENERATIONS], 0);
    source.append(&lines(15..17));
    let (_, reclassified) = classified(&store, &source.path, "carried", &owner);
    expected.push(0..17);
    assert_eq!(reclassified.event_count, 17);
    assert_eq!(*batches.lock().unwrap(), expected);
    store.assert_conserved();
}

#[test]
fn label_slots_publish_take_reinsert_and_release_conserved() {
    let source = LedgerSource::new(&lines(0..300));
    let store = fast_store();
    let owner = context_for("labels", false);
    let (handle, _native) = acquired(&store, &source.path, &owner);
    let policy = json!({"id":"ledger-labels","version":"1"});
    let first = store
        .prepare_classifier(
            &handle,
            &policy,
            &owner,
            &Cancellation::default(),
            label_bounds(),
        )
        .unwrap();
    assert_eq!(first["complete"].as_bool(), Some(false), "{first:?}");
    store.assert_conserved();
    let second = submit_labels(&store, &first, &owner);
    assert_eq!(second["complete"].as_bool(), Some(false), "{second:?}");
    assert_eq!(second["event_start"].as_u64(), Some(256));
    store.assert_conserved();
    release_cursor(&store, second["cursor"].as_str().unwrap(), &owner);
    store.assert_conserved();
    let mut reply = store
        .prepare_classifier(
            &handle,
            &policy,
            &owner,
            &Cancellation::default(),
            label_bounds(),
        )
        .unwrap();
    store.assert_conserved();
    while reply["complete"].as_bool() != Some(true) {
        reply = submit_labels(&store, &reply, &owner);
        store.assert_conserved();
    }
    let derived_handle = reply["description"]["handle"].clone();
    let derived = store.pin(&derived_handle, &owner).unwrap();
    assert_eq!(derived.event_count, 300);
    store.assert_conserved();
    release_lease(&store, &derived_handle, &owner);
    drop(derived);
    evict_unpinned(&store, &owner);
    store.assert_conserved();
}

#[test]
fn every_cursor_kind_takes_out_and_reinserts_conserved() {
    let scenario = Scenario::new(9, |index| session_line(&format!("thread-{index:04}")));
    let prompts = scenario.root.file("prompts.jsonl", &lines(0..3));
    let store = cursor_store();
    let owner = context_for("cursors", false);
    let (root, _root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let (prompt_handle, _prompt_snapshot) = acquired(&store, &prompts, &owner);
    let roots = scenario.roots();
    let ids = scenario.ids();
    let deadline = now_ms() + 120_000;
    let discovery = json!({"schema":SCHEMA,"id":"ledger-discover","operation":"discover","roots":roots,"checkpoint":null,"deadline_unix_ms":deadline,"limits":limits_json()});
    let discovered = parked_until_settled(&store, &discovery, &owner);
    assert_eq!(discovered["status"].as_str(), Some("ok"), "{discovered:?}");
    let mut rediscovery = discovery.clone();
    rediscovery.insert("checkpoint", discovered["data"]["checkpoint"].clone());
    for (kind, request) in [
        ("rediscovery", rediscovery),
        (
            "resolution",
            json!({"schema":SCHEMA,"id":"ledger-resolve","operation":"resolve","session_ids":[ids[0], ids[1]],"roots":roots,"classifier":{"id":"native","version":"1"},"deadline_unix_ms":deadline,"limits":limits_json()}),
        ),
        (
            "location",
            json!({"schema":SCHEMA,"id":"ledger-locate","operation":"locate","session_ids":ids,"roots":roots,"deadline_unix_ms":deadline,"limits":limits_json()}),
        ),
        (
            "projection",
            json!({"schema":SCHEMA,"id":"ledger-prompts","operation":"query","view":{"handle":prompt_handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"query":{"kind":"prompts","selection":"first","count":10},"deadline_unix_ms":deadline,"limits":limits_json()}),
        ),
    ] {
        let outcome = parked_until_settled(&store, &request, &owner);
        assert_eq!(
            outcome["status"].as_str(),
            Some("ok"),
            "{kind}: {outcome:?}"
        );
    }
    let built = parked_until_settled(
        &store,
        &prepare_request(&root, &[], &[], &scenario.direct()),
        &owner,
    );
    assert_eq!(built["status"].as_str(), Some("ok"), "{built:?}");
    let queried = parked_until_settled(
        &store,
        &graph_query(&built["data"]["handle"], missing_tool(), json!([])),
        &owner,
    );
    assert_eq!(queried["status"].as_str(), Some("ok"), "{queried:?}");
    assert_eq!(queried["data"]["value"].as_bool(), Some(false));
    store.assert_conserved();
    let state = store.lock_state();
    let tables = [
        ("projections", state.projections.capacity_bytes()),
        ("discoveries", state.discoveries.capacity_bytes()),
        ("checkpoints", state.checkpoints.capacity_bytes()),
        ("resolutions", state.resolutions.capacity_bytes()),
        ("locates", state.locates.capacity_bytes()),
        ("prepared_builds", state.prepared_builds.capacity_bytes()),
        ("prepared_graphs", state.prepared_graphs.capacity_bytes()),
        ("prepared_queries", state.prepared_queries.capacity_bytes()),
        ("prepared_facts", state.prepared_facts.capacity_bytes()),
        ("loads", state.loads.reserved_bytes()),
        ("deliveries", state.deliveries.capacity_bytes()),
    ];
    for (table, capacity) in tables {
        assert!(capacity > 0, "{table} never reserved a tier");
    }
    assert!(
        NativeStore::fixed_metadata_bytes(&state)
            >= tables.iter().map(|(_, capacity)| capacity).sum::<usize>(),
        "cursor table capacity is missing from the bookkeeping"
    );
}

#[test]
fn waiter_context_replacement_recharges_reservations() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let long = scenario.root.file("long.jsonl", &lines(0..6));
    let store = cursor_store();
    let owner = context_for("waiters", false);
    let mut padded = owner.clone();
    padded.insert("padding", json!("p".repeat(4096)));
    let mut response = store.request(&acquire(&long), &owner, &Cancellation::default());
    let mut resumes = 0;
    while response["status"].as_str() == Some("incomplete") {
        let context = if resumes % 2 == 0 { &padded } else { &owner };
        let before = ledger(&store);
        response = resume(&store, &response, context);
        store.assert_conserved();
        resumes += 1;
        if resumes > 1 && response["status"].as_str() == Some("incomplete") {
            let after = ledger(&store);
            if resumes % 2 == 1 {
                assert!(
                    after[PROJECTION] >= before[PROJECTION] + 4096,
                    "padded context was not charged: {before:?} -> {after:?}"
                );
            } else {
                assert!(
                    after[PROJECTION] + 4096 <= before[PROJECTION],
                    "padded context was not released: {before:?} -> {after:?}"
                );
            }
        }
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(resumes > 4, "the load finished after {resumes} resumes");
    let (root, _root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    let mut page = store.request(
        &graph_query(&graph, missing_tool(), json!([])),
        &owner,
        &Cancellation::default(),
    );
    let mut pages = 0;
    while page["cursor"].as_str().is_some() {
        page = resume(&store, &page, if pages % 2 == 0 { &padded } else { &owner });
        store.assert_conserved();
        pages += 1;
    }
    assert_eq!(page["status"].as_str(), Some("ok"), "{page:?}");
    assert_eq!(page["data"]["value"].as_bool(), Some(false));
    assert!(pages > 1, "the prepared query finished after {pages} pages");
}

#[test]
fn registry_registration_charges_its_record_once() {
    let source = LedgerSource::new(&lines(0..2));
    let store = fast_store();
    let owner = context_for("registry", false);
    store.assert_conserved();
    let before = settled(&store);
    let specs = json!([{"name":"fetch_source","behaves_like":"Read","span_edit":null}]);
    let fingerprint = store.register_tool_registry(&specs, &owner).unwrap();
    store.assert_conserved();
    let registered = settled(&store);
    assert!(registered[TOTAL] > before[TOTAL]);
    assert_eq!(registered[PENDING], before[PENDING]);
    assert_eq!(
        store.register_tool_registry(&specs, &owner).unwrap(),
        fingerprint
    );
    store.assert_conserved();
    assert_eq!(settled(&store), registered);
    let mut scoped = owner.clone();
    scoped.insert("registry_generation", json!(fingerprint));
    let _scoped = acquired(&store, &source.path, &scoped);
    store.assert_conserved();
}

#[test]
fn carried_stage_and_label_seeds_share_one_anchor() {
    let source = LedgerSource::new(&lines(0..4));
    let store = fast_store();
    let owner = context_for("seeds", false);
    let batches = recording_classifier(&store, "overlap");
    let _carried = classified(&store, &source.path, "overlap", &owner);
    source.append(&lines(4..8));
    let (native, appended) = acquired(&store, &source.path, &owner);
    store.assert_conserved();
    let anchored = settled(&store)[INDEXES];
    let one_event = appended
        .chunks
        .iter()
        .flat_map(|chunk| chunk.entry_charges.iter())
        .map(|charge| charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
        .max()
        .unwrap();
    let mut staged = store.request(
        &classifier_acquire(&source.path, "overlap", one_event),
        &owner,
        &Cancellation::default(),
    );
    for _ in 0..16 {
        if batches.lock().unwrap().last() == Some(&(4..5)) {
            break;
        }
        staged = resume(&store, &staged, &owner);
    }
    assert_eq!(batches.lock().unwrap().last(), Some(&(4..5)));
    assert_eq!(staged["status"].as_str(), Some("incomplete"), "{staged:?}");
    store.assert_conserved();
    assert_eq!(settled(&store)[INDEXES], anchored);
    let label = store
        .prepare_classifier(
            &native,
            &json!({"id":"overlap","version":"1"}),
            &owner,
            &Cancellation::default(),
            label_bounds(),
        )
        .unwrap();
    assert_eq!(label["complete"].as_bool(), Some(false), "{label:?}");
    assert_eq!(label["event_start"].as_u64(), Some(4));
    store.assert_conserved();
    assert_eq!(settled(&store)[INDEXES], anchored);
    release_cursor(&store, label["cursor"].as_str().unwrap(), &owner);
    store.assert_conserved();
    assert_eq!(settled(&store)[INDEXES], anchored);
    release_cursor(&store, staged["cursor"].as_str().unwrap(), &owner);
    store.assert_conserved();
    evict_unpinned(&store, &owner);
    store.assert_conserved();
}

#[test]
fn prepared_facts_shared_by_cache_graphs_builds_and_slices_stay_conserved() {
    let scenario = Scenario::new(10, |index| prompt_line(&format!("thread-{index:04}"), 2048));
    let store = store_with(4096, 2048, &[("max_prepared_fact_memory_bytes", 8 * 1024)]);
    let owner = context_for("facts", false);
    let (root, _root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let direct = scenario.direct();
    let first = prepared_graph(&store, &root, &direct[..1], &owner);
    let second = prepared_graph(&store, &root, &direct[..1], &owner);
    assert_ne!(first["graph_id"], second["graph_id"]);
    let build = store.request(
        &prepare_request(&root, &[], &[], &direct),
        &owner,
        &Cancellation::default(),
    );
    assert!(parked(&build));
    store.assert_conserved();
    for (graph, selectors) in [
        (&first, json!([])),
        (&first, json!([{"kind":"current_turn"}])),
        (&second, json!([{"kind":"current_turn"}])),
    ] {
        let reply = drive(
            &store,
            store.request(
                &graph_query(graph, missing_tool(), selectors),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(reply["data"]["value"].as_bool(), Some(false));
    }
    let everything = prepared_graph(&store, &root, &direct, &owner);
    let reply = drive(
        &store,
        store.request(
            &graph_query(&everything, missing_tool(), json!([])),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
    assert_eq!(reply["data"]["value"].as_bool(), Some(false));
    for graph in [&first, &second, &everything] {
        release_graph(&store, graph, &owner);
        store.assert_conserved();
    }
    release_cursor(&store, build["cursor"].as_str().unwrap(), &owner);
    store.assert_conserved();
}

#[test]
fn failed_and_parked_classifier_stages_stay_conserved() {
    let source = LedgerSource::new(&lines(0..3));
    let store = NativeStore::new(&json!({"max_pending_loads":1,"reserved_hook_loads":0,"max_events_per_step":1,"max_retained_bytes":CAP,"reserved_hook_accounted_bytes":HOOK_BYTES})).unwrap();
    let owner = context_for("stages", false);
    let _native = acquired(&store, &source.path, &owner);
    store
        .register_classifier(
            "failing",
            "1",
            Arc::new(|_, _| {
                Err(SnapshotError::new(
                    Status::InvalidRequest,
                    "classifier raised",
                ))
            }),
        )
        .unwrap();
    let failed = store.request(
        &classifier_acquire(&source.path, "failing", 1024 * 1024),
        &owner,
        &Cancellation::default(),
    );
    assert_eq!(
        failed["status"].as_str(),
        Some("invalid_request"),
        "{failed:?}"
    );
    store.assert_conserved();
    settled(&store);
    store.assert_conserved();
    recording_classifier(&store, "parked");
    let waiting = recording_classifier(&store, "waiting");
    let parked_stage = store.request(
        &classifier_acquire(&source.path, "parked", 1024 * 1024),
        &owner,
        &Cancellation::default(),
    );
    assert!(parked(&parked_stage));
    store.assert_conserved();
    assert!(settled(&store)[PENDING] > 0);
    let contended = store.request(
        &classifier_acquire(&source.path, "waiting", 1024 * 1024),
        &owner,
        &Cancellation::default(),
    );
    assert_eq!(
        contended["status"].as_str(),
        Some("retained_limit"),
        "{contended:?}"
    );
    store.assert_conserved();
    release_cursor(&store, parked_stage["cursor"].as_str().unwrap(), &owner);
    store.assert_conserved();
    settled(&store);
    store.assert_conserved();
    let (_, completed) = classified(&store, &source.path, "waiting", &owner);
    assert_eq!(completed.event_count, 3);
    assert_eq!(*waiting.lock().unwrap(), vec![0..1, 1..2, 2..3]);
    store.assert_conserved();
}

#[test]
fn load_growth_publication_and_failures_stay_conserved() {
    let source = LedgerSource::new(&lines(0..4));
    let store = store_with(SLOW_READ_STEP, 2048, &[("max_entry_bytes", 1024)]);
    let owner = context_for("loads", false);
    let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    let mut steps = 0;
    while response["status"].as_str() == Some("incomplete") {
        store.assert_conserved();
        assert_eq!(ledger(&store)[LOADS], 1);
        response = resume(&store, &response, &owner);
        steps += 1;
    }
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(steps > 4, "the load published after {steps} steps");
    store.assert_conserved();
    let oversized = source.file("oversized.jsonl", &prompt_line("oversized", 2048));
    let failed = settle(
        &store,
        store.request(&acquire(&oversized), &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(failed["status"].as_str(), Some("entry_limit"), "{failed:?}");
    store.assert_conserved();
    assert_eq!(settled(&store)[LOADS], 0);
    store.assert_conserved();
    let truncated = source.file("truncated.jsonl", &lines(10..14));
    let started = store.request(&acquire(&truncated), &owner, &Cancellation::default());
    assert!(parked(&started));
    std::fs::write(&truncated, "").unwrap();
    let changed = settle(&store, resume(&store, &started, &owner), &owner);
    assert_eq!(changed["status"].as_str(), Some("changed"), "{changed:?}");
    store.assert_conserved();
    assert_eq!(settled(&store)[LOADS], 0);
    store.assert_conserved();
}

#[test]
fn registered_prepare_graph_admits_exactly_before_publication() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        assert_admission_boundary(
            "registered prepare_graph",
            &|| registered_graph_fixture(&scenario, background),
            &|fixture: &Fixture| admitted(&submit(fixture), "ok"),
            REPLY_RESERVATION,
        );
    }
}

#[test]
fn direct_prepare_graph_admits_exactly_before_publication() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        assert_admission_boundary(
            "direct prepare_graph",
            &|| direct_graph_fixture(&scenario, background),
            &|fixture: &Fixture| admitted(&submit(fixture), "ok"),
            REPLY_RESERVATION,
        );
    }
}

#[test]
fn stored_prepared_build_admits_exactly_before_publication() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        assert_admission_boundary(
            "store_prepared_build",
            &|| direct_graph_fixture(&scenario, background),
            &|fixture: &Fixture| parked(&submit(fixture)),
            REPLY_RESERVATION,
        );
    }
}

#[test]
fn stored_prepared_build_admits_its_request_exactly() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let padding = 64 * 1024;
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let build = |padding: usize| {
            let mut fixture = direct_graph_fixture(&scenario, background);
            fixture
                .request
                .insert("padding", json!("p".repeat(padding)));
            fixture
        };
        let small = exact_headroom(&|| build(1), &attempt);
        let large = exact_headroom(&|| build(padding), &attempt);
        assert_eq!(
            large - small,
            value_bytes(&build(padding).request) - value_bytes(&build(1).request),
            "the parked build's request is not charged by its bytes"
        );
        assert_boundary_at("store_prepared_build", &build(padding), &attempt, large);
    }
}

fn reserved_before_the_last_admission(site: &str, traced: &[Trace], bytes: usize) {
    let reserved = traced
        .iter()
        .position(|trace| *trace == Trace::Reserved(bytes))
        .unwrap_or_else(|| panic!("{site}: nothing reserved {bytes} bytes: {traced:?}"));
    let admitted = traced
        .iter()
        .rposition(|trace| matches!(trace, Trace::Admitted(_)))
        .unwrap_or_else(|| panic!("{site}: nothing was admitted: {traced:?}"));
    assert!(
        reserved < admitted,
        "{site}: the reservation followed the publication admission: {traced:?}"
    );
}

fn constructed_graph_bytes(state: &StoreState, graph_id: &str, graph: &PreparedGraph) -> usize {
    let key = state
        .prepared_graphs
        .iter()
        .find(|(key, _)| key.as_str() == graph_id)
        .map(|(key, _)| key.capacity())
        .expect("published graph key");
    let shared_sources = state
        .warm_memberships
        .values()
        .any(|membership| Arc::ptr_eq(&membership.members, &graph.sources));
    let shared_dirs = state
        .warm_memberships
        .values()
        .any(|membership| Arc::ptr_eq(&membership.sidechain_dirs, &graph.sidechain_dirs));
    let sources = if shared_sources {
        0
    } else {
        graph.sources.len() * size_of::<PreparedSourceRef>()
            + graph
                .sources
                .iter()
                .map(|source| source.path.as_os_str().len())
                .sum::<usize>()
    };
    let dirs = if shared_dirs {
        0
    } else {
        graph.sidechain_dirs.len() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + graph
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.as_os_str().len())
                .sum::<usize>()
    };
    key + size_of::<PreparedGraph>()
        + graph.claimant.capacity()
        + graph.registry_generation.capacity()
        + graph.admission.capacity()
        + graph.revision.capacity()
        + value_bytes(&graph.authority)
        + value_bytes(&graph.root_handle)
        + value_bytes(&graph.classifier)
        + graph.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
        + graph
            .stamps
            .iter()
            .map(|(path, _)| path.as_os_str().len())
            .sum::<usize>()
        + graph.root_slices.reserved_bytes()
        + graph
            .root_slices
            .keys()
            .map(String::capacity)
            .sum::<usize>()
        + sources
        + dirs
}

#[test]
fn registered_prepare_graph_reserves_its_stamps_and_paths_before_construction() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = registered_graph_fixture(&scenario, background);
        traced(&fixture.store);
        let response = submit(&fixture);
        assert!(admitted(&response, "ok"), "{response:?}");
        let traced = traced(&fixture.store);
        let graph_id = response["data"]["handle"]["graph_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[&graph_id].lock().unwrap();
        reserved_before_the_last_admission(
            "registered prepare_graph",
            &traced,
            constructed_graph_bytes(&state, &graph_id, &graph),
        );
    }
}

#[test]
fn direct_prepare_graph_reserves_its_source_buffers_before_construction() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = direct_graph_fixture(&scenario, background);
        traced(&fixture.store);
        let response = submit(&fixture);
        assert!(admitted(&response, "ok"), "{response:?}");
        let traced = traced(&fixture.store);
        let graph_id = response["data"]["handle"]["graph_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[&graph_id].lock().unwrap();
        reserved_before_the_last_admission(
            "direct prepare_graph",
            &traced,
            constructed_graph_bytes(&state, &graph_id, &graph),
        );
    }
}

#[test]
fn root_facts_are_reserved_at_their_entry_bytes_before_parsing() {
    let scenario = Scenario::new(0, |_| String::new());
    for background in [false, true] {
        let store = prepared_store();
        let owner = context_for("facts", background);
        let (root, snapshot) = acquired(&store, &scenario.root.path, &owner);
        traced(&store);
        let prepared = drive(
            &store,
            store.request(
                &prepare_request(&root, &[], &[], &[]),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
        let entries: usize = snapshot
            .chunks
            .iter()
            .map(|chunk| {
                chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes
            })
            .sum();
        reserved_before_the_last_admission("root facts", &traced(&store), entries);
        let facts = store.lock_state().prepared_facts[&snapshot.stamp.identity]
            .facts
            .accounted_bytes();
        assert!(
            entries >= facts,
            "root facts: the {entries}-byte entry reservation does not cover the {facts}-byte facts DOM"
        );
    }
}

#[test]
fn root_slices_are_reserved_at_the_root_facts_bytes_before_preparation() {
    let scenario = Scenario::new(0, |_| String::new());
    for background in [false, true] {
        let fixture = slice_fixture(&scenario, background);
        let graph_id = fixture.request["handle"]["graph_id"].as_str().unwrap();
        let (root_facts, existing) = {
            let state = fixture.store.lock_state();
            let graph = state.prepared_graphs[graph_id].lock().unwrap();
            (
                graph.root_facts.accounted_bytes(),
                graph.root_slices.keys().cloned().collect::<Vec<_>>(),
            )
        };
        traced(&fixture.store);
        let response = submit(&fixture);
        assert!(admitted(&response, "ok"), "{response:?}");
        reserved_before_the_last_admission("root slice", &traced(&fixture.store), root_facts);
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[graph_id].lock().unwrap();
        let slice = graph
            .root_slices
            .iter()
            .find(|(key, _)| !existing.contains(*key))
            .map(|(_, facts)| facts.accounted_bytes())
            .expect("published root slice");
        assert!(
            root_facts >= slice,
            "root slice: the {root_facts}-byte root facts reservation does not cover the {slice}-byte slice"
        );
    }
}

#[test]
fn prepared_query_page_admits_exactly_before_reinsertion() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        assert_admission_boundary(
            "prepared_query_page",
            &|| input_page_fixture(&scenario, background, 0),
            &|fixture: &Fixture| parked(&submit(fixture)),
            REPLY_RESERVATION,
        );
    }
}

#[test]
fn root_slice_admits_exactly_before_insertion() {
    let scenario = Scenario::new(0, |_| String::new());
    for background in [false, true] {
        assert_admission_boundary(
            "root slice",
            &|| slice_fixture(&scenario, background),
            &|fixture: &Fixture| admitted(&submit(fixture), "ok"),
            REPLY_RESERVATION,
        );
    }
}

#[test]
fn classifier_publication_admits_exactly_before_registering_the_generation() {
    let source = LedgerSource::new(&lines(0..2));
    let classifier = "c".repeat(16 * 1024);
    for background in [false, true] {
        assert_admission_boundary(
            "classifier publication",
            &|| interrupted_classification(&source, &classifier, background),
            &classification_published,
            8 * 1024,
        );
    }
}

#[test]
fn classifier_stage_admits_its_lineage_key_exactly() {
    let source = LedgerSource::new(&lines(0..2));
    let long = "c".repeat(16 * 1024);
    for background in [false, true] {
        let small = exact_headroom(
            &|| classifier_stage_fixture(&source, "c", background),
            &stage_created,
        );
        let large = exact_headroom(
            &|| classifier_stage_fixture(&source, &long, background),
            &stage_created,
        );
        assert_eq!(
            large - small,
            long.len() - 1,
            "the classifier stage key is not charged by its length"
        );
        let fixture = classifier_stage_fixture(&source, &long, background);
        let before = fixture.store.lock_state().ledger.classifier;
        assert_boundary_at("classifier stage", &fixture, &stage_created, large);
        let state = fixture.store.lock_state();
        let (key, slot) = state
            .classifier_stages
            .iter()
            .next()
            .expect("created classifier stage");
        assert!(key.capacity() > 16 * 1024);
        assert_eq!(
            state.ledger.classifier - before,
            key.capacity() + slot.accounted.load(Ordering::Acquire),
            "the classifier gauge misses the stage key"
        );
    }
}

#[test]
fn startup_footprint_bounds_the_retained_cap() {
    let footprint = NativeStore::new(&json!({}))
        .unwrap()
        .retained_accounted_bytes();
    assert!(footprint > 0);
    let with_cap = |cap: usize| NativeStore::new(&json!({"max_retained_bytes": cap}));
    assert_eq!(
        with_cap(footprint - 1)
            .err()
            .expect("a cap below the startup footprint")
            .status,
        Status::InvalidRequest
    );
    assert_eq!(
        with_cap(footprint).unwrap().retained_accounted_bytes(),
        footprint
    );
    assert_eq!(
        with_cap(footprint + 1).unwrap().retained_accounted_bytes(),
        footprint
    );
}

#[test]
fn default_startup_footprint_is_the_pre_sized_tables_and_the_default_registries() {
    let store = NativeStore::new(&json!({})).unwrap();
    let registries = {
        let state = store.lock_state();
        assert_eq!(
            state.deliveries.capacity_bytes(),
            PRE_SIZED_SLOTS * Ledgered::<Arc<str>, Delivery>::entry_bytes()
        );
        assert_eq!(
            state.expired_prepared_queries.capacity_bytes(),
            PRE_SIZED_SLOTS * Ledgered::<String, (String, u64)>::entry_bytes()
        );
        state.registries.capacity_bytes()
            + state.registries.charged()
            + state.ledger.shared.table_bytes()
            + state.ledger.shared.indexes()
    };
    assert_eq!(
        store.retained_accounted_bytes(),
        size_of::<StoreState>()
            + PRE_SIZED_SLOTS
                * (Ledgered::<Arc<str>, Delivery>::entry_bytes()
                    + Ledgered::<String, (String, u64)>::entry_bytes())
            + registries
    );
}

#[test]
fn advance_publication_admits_exactly_before_registering_the_generation() {
    let source = LedgerSource::new(&line("anchor"));
    let deep = deep_source(&source, &line("advance"));
    for background in [false, true] {
        let requests = publishing_request(&deep, background);
        let build = || parked_load(&deep, background, requests);
        let exact = exact_headroom(&build, &advance_published);
        assert!(
            exact > 5 * SLOW_READ_STEP,
            "advance publication needs {exact} bytes, not past the step reservation"
        );
        let released = {
            let twin = build();
            let filler = fill_to(&twin.store, &twin.owner, exact - 1);
            release_cursor(
                &twin.store,
                twin.request["cursor"].as_str().unwrap(),
                &twin.owner,
            );
            drop(filler);
            settled(&twin.store)
        };
        let refused = build();
        let cap = cap_for(&refused.owner);
        let generations = ledger(&refused.store)[GENERATIONS];
        let filler = fill_to(&refused.store, &refused.owner, exact - 1);
        assert!(!advance_published(&refused));
        refused.store.assert_conserved();
        assert!(audited(&refused.store)[TOTAL] <= cap);
        drop(filler);
        assert_eq!(settled(&refused.store), released);
        refused.store.assert_conserved();
        let retried = drive(
            &refused.store,
            refused
                .store
                .request(&acquire(&deep), &refused.owner, &Cancellation::default()),
            &refused.owner,
        );
        assert_eq!(retried["status"].as_str(), Some("ok"), "{retried:?}");
        assert_eq!(ledger(&refused.store)[GENERATIONS], generations + 1);
        let fitted = build();
        let generations = ledger(&fitted.store)[GENERATIONS];
        let filler = fill_to(&fitted.store, &fitted.owner, exact);
        assert!(advance_published(&fitted));
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        assert_eq!(ledger(&fitted.store)[GENERATIONS], generations + 1);
        drop(filler);
        fitted.store.assert_conserved();
    }
}

#[test]
fn lease_issue_admits_exactly_its_record() {
    let source = LedgerSource::new(&lines(0..2));
    for background in [false, true] {
        let build = || lease_fixture(&source, background, false);
        let exact = exact_headroom(&build, &issued);
        assert_boundary_at("lease issue", &build(), &issued, exact);
        let growth = assert_exact_growth("lease issue", &build(), &issued, exact, &|store| {
            lease_table(store).2
        });
        assert_eq!(growth, 0, "an unfilled lease table grew");
        let refused = build();
        let leases = lease_table(&refused.store);
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!issued(&refused));
        }
        assert_eq!(
            lease_table(&refused.store),
            leases,
            "a refused lease was left behind"
        );
        assert!(
            issued(&refused),
            "the store was unusable after a refused lease"
        );
        refused.store.assert_conserved();
    }
}

fn delivery_bytes(store: &NativeStore) -> usize {
    store.lock_state().deliveries.charged()
}

#[test]
fn issued_lease_pledges_its_delivery_and_converts_it_without_admission() {
    let source = LedgerSource::new(&lines(0..2));
    for background in [false, true] {
        let fixture = lease_fixture(&source, background, false);
        let token = fixture.store.token("lease");
        let acquired = {
            let mut state = fixture.store.lock_state();
            fixture
                .store
                .issue(
                    &mut state,
                    Arc::clone(&fixture.pins[0]),
                    fixture.request.clone(),
                    &fixture.owner,
                    now_ms() + 120_000,
                    token.clone(),
                    0,
                )
                .unwrap()
        };
        let (pledged, charged, delivered, deliveries) = {
            let state = fixture.store.lock_state();
            (
                state.leases[&token].delivery,
                state.leases.charged(),
                state.deliveries.charged() + state.deliveries_expiry.index_bytes(),
                state.deliveries.len(),
            )
        };
        assert!(
            pledged > 0,
            "the issued lease pledged nothing for its delivery"
        );
        traced(&fixture.store);
        fixture
            .store
            .track_delivery(
                &json!({"id":"ledger-delivery","status":"ok","data":acquired}),
                &fixture.owner,
                true,
            )
            .unwrap();
        let admitted = traced(&fixture.store);
        assert!(
            admitted.is_empty(),
            "converting the delivery pledge admitted or allocated again: {admitted:?}"
        );
        fixture.store.assert_conserved();
        let state = fixture.store.lock_state();
        assert!(state.leases[&token].exposed);
        assert_eq!(state.leases[&token].delivery, 0);
        assert_eq!(charged - state.leases.charged(), pledged);
        assert_eq!(state.deliveries.len(), deliveries + 1);
        assert_eq!(
            state.deliveries.charged() + state.deliveries_expiry.index_bytes() - delivered,
            pledged,
            "the delivery record differs from its pledge"
        );
    }
}

#[test]
fn parked_cursor_pledges_its_delivery_and_converts_it_without_admission() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = input_page_fixture(&scenario, background, 0);
        let (capacity, delivered) = {
            let state = fixture.store.lock_state();
            (
                state.prepared_queries.capacity_bytes(),
                state.deliveries.charged() + state.deliveries_expiry.index_bytes(),
            )
        };
        traced(&fixture.store);
        let response = submit(&fixture);
        assert!(parked(&response), "{response:?}");
        let admitted: Vec<usize> = traced(&fixture.store)
            .into_iter()
            .filter_map(|trace| match trace {
                Trace::Admitted(bytes) => Some(bytes),
                Trace::Allocated(_) | Trace::Reserved(_) => None,
            })
            .collect();
        fixture.store.assert_conserved();
        let token = response["cursor"].as_str().unwrap().to_owned();
        let pledge = Delivery::cursor_pledge(fixture.owner["claimant"].as_str().unwrap(), &token);
        let state = fixture.store.lock_state();
        assert_eq!(
            state.prepared_queries.pledged(&token),
            0,
            "the delivered cursor kept its pledge"
        );
        assert_eq!(
            state.deliveries.charged() + state.deliveries_expiry.index_bytes() - delivered,
            pledge,
            "the delivery record differs from its pledge"
        );
        assert_eq!(
            admitted.last().copied(),
            Some(
                charged_bytes(&token, &state.prepared_queries[&token])
                    + state.prepared_queries.capacity_bytes()
                    - capacity
                    + pledge
            ),
            "the park admission did not cover the cursor, its table growth, and its delivery pledge: {admitted:?}"
        );
    }
}

#[test]
fn redelivered_cursor_admits_exactly_its_record_and_releases_a_refused_cursor() {
    let source = LedgerSource::new(&lines(0..8));
    for background in [false, true] {
        let build = || parked_load(&source.path, background, 2);
        let tracked = |fixture: &Fixture| {
            let response = json!({"id":"ledger-delivery","status":"incomplete","cursor":fixture.request["cursor"]});
            match fixture
                .store
                .track_delivery(&response, &fixture.owner, false)
            {
                Ok(()) => true,
                Err(error) if error.status == Status::RetainedLimit => false,
                Err(error) => panic!("delivery tracking failed outside admission: {error:?}"),
            }
        };
        let exact = exact_headroom(&build, &tracked);
        assert!(exact > 0);
        let refused = build();
        let cap = cap_for(&refused.owner);
        let token = refused.request["cursor"].as_str().unwrap().to_owned();
        let deliveries = {
            let state = refused.store.lock_state();
            assert!(state.waiters.contains_key(&token));
            state.deliveries.len()
        };
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            traced(&refused.store);
            assert!(
                !tracked(&refused),
                "one byte over the exact fit tracked the delivery"
            );
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap);
            assert!(
                !traced(&refused.store)
                    .iter()
                    .any(|trace| matches!(trace, Trace::Allocated(_))),
                "the refused delivery allocated retained bookkeeping"
            );
            let state = refused.store.lock_state();
            assert_eq!(
                state.deliveries.len(),
                deliveries,
                "the refused delivery landed"
            );
            assert!(
                !state.waiters.contains_key(&token),
                "the refused delivery left its cursor issued"
            );
            assert!(state.loads.is_empty(), "the released cursor kept its load");
        }
        let fitted = build();
        let growth = assert_exact_growth(
            "delivery tracking",
            &fitted,
            &tracked,
            exact,
            &delivery_bytes,
        );
        assert_eq!(
            growth,
            size_of::<(u64, Arc<str>)>(),
            "the pre-sized delivery table grew past its deadline node"
        );
        assert_eq!(fitted.store.lock_state().deliveries.len(), deliveries + 1);
    }
}

#[test]
fn lease_table_growth_is_admitted_before_the_lease_is_issued() {
    let source = LedgerSource::new(&lines(0..2));
    for background in [false, true] {
        let build = || lease_fixture(&source, background, true);
        let exact = exact_headroom(&build, &issued);
        assert_boundary_at("lease table growth", &build(), &issued, exact);
        let fitted = build();
        let (_, reserved, _, capacity_bytes) = lease_table(&fitted.store);
        traced(&fitted.store);
        let growth = assert_exact_growth("lease table growth", &fitted, &issued, exact, &|store| {
            lease_table(store).2
        });
        let traced = traced(&fitted.store);
        assert_admitted_before_allocating("lease table growth", &traced);
        let (len, grown, _, grown_bytes) = lease_table(&fitted.store);
        assert!(grown > reserved, "the full lease table did not grow");
        assert!(len <= grown);
        assert!(
            traced.contains(&Trace::Allocated(grown)),
            "the lease table did not report its new tier: {traced:?}"
        );
        assert_eq!(
            growth,
            grown_bytes - capacity_bytes,
            "lease table growth is not its added slots"
        );
        let refused = build();
        let leases = lease_table(&refused.store);
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!issued(&refused));
        }
        assert_eq!(
            lease_table(&refused.store),
            leases,
            "a refused lease grew its table"
        );
        assert!(
            issued(&refused),
            "the store was unusable after a refused lease"
        );
        refused.store.assert_conserved();
    }
}

#[test]
fn location_heap_growth_is_admitted_before_locations_are_remembered() {
    let source = LedgerSource::new(&line("locate"));
    for background in [false, true] {
        let build = || location_fixture(&source, background);
        let exact = exact_headroom(&build, &remembered);
        assert_boundary_at("location heap growth", &build(), &remembered, exact);
        let fitted = build();
        let (_, reserved, heap_bytes) = location_heap(&fitted.store);
        let added = location_bytes(&fitted);
        let growth = {
            let _filler = fill_to(&fitted.store, &fitted.owner, exact);
            let before = bookkeeping(&fitted.store);
            traced(&fitted.store);
            assert!(remembered(&fitted));
            assert_admitted_before_allocating("location heap growth", &traced(&fitted.store));
            fitted.store.assert_conserved();
            assert!(audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner));
            bookkeeping(&fitted.store) - before
        };
        let (len, grown, grown_bytes) = location_heap(&fitted.store);
        assert!(grown > reserved, "the full location heap did not grow");
        assert!(len <= grown);
        assert_eq!(growth, grown_bytes - heap_bytes, "growth beyond the heap");
        assert_eq!(
            exact,
            added + growth,
            "headroom is not the charge plus heap growth"
        );
        let refused = build();
        let heap = location_heap(&refused.store);
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!remembered(&refused));
        }
        assert_eq!(
            location_heap(&refused.store),
            heap,
            "a refused location grew its heap"
        );
        assert!(
            remembered(&refused),
            "the store was unusable after a refused location"
        );
        refused.store.assert_conserved();
    }
}

#[test]
fn generation_and_anchor_table_growth_is_admitted_before_classifier_publication() {
    let source = LedgerSource::new(&lines(0..2));
    let extras: Vec<_> = (0..8)
        .map(|index| LedgerSource::new(&lines(index * 2 + 2..index * 2 + 4)))
        .collect();
    let classifier = "g".repeat(16 * 1024);
    for background in [false, true] {
        let build = || padded_classification(&source, &extras, &classifier, background);
        let exact = exact_headroom(&build, &classification_published);
        assert!(exact > 8 * 1024);
        assert_boundary_at(
            "padded classifier publication",
            &build(),
            &classification_published,
            exact,
        );
        let fitted = build();
        let (generations, anchors) = publication_tables(&fitted.store);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        let before = bookkeeping(&fitted.store);
        traced(&fitted.store);
        assert!(classification_published(&fitted));
        assert_admitted_before_allocating("padded classifier publication", &traced(&fitted.store));
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner));
        let (grown_generations, grown_anchors) = publication_tables(&fitted.store);
        assert!(
            grown_generations > generations,
            "the full generation table did not grow"
        );
        assert!(
            grown_anchors > anchors,
            "the full anchor table did not grow"
        );
        assert!(bookkeeping(&fitted.store) > before);
    }
}

fn parked_slot(fixture: &Fixture) -> Arc<LoadSlot> {
    fixture
        .store
        .lock_state()
        .loads
        .values()
        .next()
        .cloned()
        .expect("parked load")
}

fn pin_prepared_load(fixture: &Fixture) -> Arc<LoadSlot> {
    let slot = parked_slot(fixture);
    fixture.store.lock_state().insert_prepared_load(
        slot.stamp.identity,
        Arc::clone(&slot),
        now_ms(),
    );
    slot
}

fn codex_lines() -> String {
    [
        r#"{"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"s","cwd":"/tmp"}}"#,
        r#"{"timestamp":"2026-01-02T03:04:06Z","type":"event_msg","payload":{"type":"agent_message","message":"recent"}}"#,
    ]
    .map(|line| format!("{line}\n"))
    .concat()
}

fn warm_root_request(path: &Path) -> Value {
    json!({"schema":SCHEMA,"id":"ledger-warm-root","operation":"warm_root","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn warm_root_fixture(source: &LedgerSource, warmed_once: bool) -> Fixture {
    let store = slow_store();
    let owner = context_for("warm-root", true);
    let request = warm_root_request(&source.path);
    if warmed_once {
        let parked = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(parked["status"].as_str(), Some("ok"), "{parked:?}");
        assert_eq!(parked["data"]["complete"].as_bool(), Some(false));
        assert!(!store.lock_state().prepared_loads.is_empty());
    }
    Fixture {
        store,
        owner,
        request,
        pins: Vec::new(),
    }
}

fn codex_acquired(fixture: &Fixture) -> (Value, Arc<TranscriptSnapshot>) {
    let acquired = drive(
        &fixture.store,
        fixture
            .store
            .request(&fixture.request, &fixture.owner, &Cancellation::default()),
        &fixture.owner,
    );
    let handle = ok_handle(&acquired);
    let snapshot = fixture.store.pin(&handle, &fixture.owner).unwrap();
    assert_eq!(snapshot.provider, Provider::Codex);
    assert!(snapshot.codex_raw.is_some());
    (acquired["data"].clone(), snapshot)
}

fn finished_recent_codex(fixture: &Fixture, acquired: &Value) -> bool {
    let stamp =
        SourceStamp::of(&std::fs::metadata(fixture.request["path"].as_str().unwrap()).unwrap());
    match fixture.store.finish_prepared_source(
        stamp,
        acquired,
        &fixture.owner,
        &work_bounds(),
        &Cancellation::default(),
    ) {
        Ok(PreparedSourceOutcome::Ready { .. }) => {}
        Ok(PreparedSourceOutcome::Pending(cursor)) => panic!("prepared source parked: {cursor}"),
        Err(error) => panic!("prepared source completion failed: {error:?}"),
    }
    fixture
        .store
        .lock_state()
        .recent_codex
        .contains_key(&stamp.identity)
}

#[test]
fn refused_publication_keeps_the_parsed_result_unpublished_until_the_retry_admits_it() {
    let source = LedgerSource::new(&line("anchor"));
    let deep = deep_source(&source, &line("retry"));
    for background in [false, true] {
        let requests = publishing_request(&deep, background);
        let build = || parked_load(&deep, background, requests);
        let exact = exact_headroom(&build, &advance_published);
        let refused = build();
        let slot = pin_prepared_load(&refused);
        let cap = cap_for(&refused.owner);
        let generations = ledger(&refused.store)[GENERATIONS];
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!advance_published(&refused));
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap);
        }
        let parsed = slot
            .work
            .lock()
            .unwrap()
            .result
            .clone()
            .expect("the parsed result survives the refused publication");
        {
            let state = refused.store.lock_state();
            assert!(!state.generations.contains_key(&snapshot_key(&parsed)));
            assert!(state.latest.get(&slot.stamp.identity).is_none());
            assert!(state.escaped_chunks.is_empty());
            assert!(state.waiters.is_empty());
            assert!(state.prepared_loads.contains_key(&slot.stamp.identity));
        }
        assert_eq!(settled(&refused.store)[GENERATIONS], generations);
        assert!(Arc::ptr_eq(&parked_slot(&refused), &slot));
        let retried = drive(
            &refused.store,
            refused
                .store
                .request(&acquire(&deep), &refused.owner, &Cancellation::default()),
            &refused.owner,
        );
        let handle = ok_handle(&retried);
        assert_eq!(retried["usage"]["source_bytes_read"].as_u64(), Some(0));
        assert!(retried["usage"]["inflight_joins"].as_u64().unwrap() >= 1);
        let pinned = refused.store.pin(&handle, &refused.owner).unwrap();
        assert!(
            Arc::ptr_eq(&pinned, &parsed),
            "the retry re-parsed instead of publishing the parsed result"
        );
        refused.store.assert_conserved();
        assert!(audited(&refused.store)[TOTAL] <= cap);
        assert_eq!(ledger(&refused.store)[GENERATIONS], generations + 1);
        {
            let state = refused.store.lock_state();
            assert!(state.generations.contains_key(&snapshot_key(&parsed)));
            assert_eq!(state.escaped_chunks.len(), parsed.chunks.len());
            assert_eq!(slot.ledgered_bytes(), 0);
        }
        let charge = chunk_charge(&parsed.chunks[0]);
        let rows = Arc::clone(&parsed.chunks[0].entries);
        release_lease(&refused.store, &handle, &refused.owner);
        drop(pinned);
        drop(parsed);
        drop(slot);
        refused.store.lock_state().prepared_loads.clear();
        evict_unpinned(&refused.store, &refused.owner);
        refused.store.assert_conserved();
        let escaped = ledger(&refused.store);
        assert_eq!(escaped[GENERATIONS], generations);
        assert!(escaped[ENTRIES] >= charge, "{escaped:?}");
        drop(rows);
        refused.store.assert_conserved();
        assert!(ledger(&refused.store)[ENTRIES] < escaped[ENTRIES]);
    }
}

fn published_follower(path: &Path, background: bool, requests: usize) -> Fixture {
    let parked = parked_load(path, background, requests);
    let slot = pin_prepared_load(&parked);
    let completed = drive(
        &parked.store,
        resume(&parked.store, &parked.request, &parked.owner),
        &parked.owner,
    );
    let snapshot = parked
        .store
        .pin(&ok_handle(&completed), &parked.owner)
        .unwrap();
    {
        let mut state = parked.store.lock_state();
        assert!(state.remove_latest(&slot.stamp.identity).is_some());
        assert!(state.generations.contains_key(&snapshot_key(&snapshot)));
        assert!(Arc::ptr_eq(
            state.loads.get(&slot.stamp.identity).expect("pinned load"),
            &slot
        ));
        for inode in 1u64.. {
            if state.latest.len() == state.latest.reserved() {
                break;
            }
            state.insert_latest(
                SourceIdentity {
                    device: 0,
                    inode,
                    window_base: 0,
                },
                Arc::clone(&snapshot),
            );
        }
        assert!(state.latest.growth(1) > 0);
    }
    Fixture {
        store: parked.store,
        owner: parked.owner,
        request: acquire(path),
        pins: vec![snapshot],
    }
}

#[test]
fn latest_cache_write_is_skipped_when_only_its_table_growth_does_not_fit() {
    let source = LedgerSource::new(&line("anchor"));
    let deep = deep_source(&source, &line("latest"));
    let identity = SourceStamp::of(&std::fs::metadata(&deep).unwrap()).identity;
    for background in [false, true] {
        let requests = publishing_request(&deep, background);
        let build = || published_follower(&deep, background, requests);
        let joined = |fixture: &Fixture| {
            let response = submit(fixture);
            let joined = admitted(&response, "ok");
            if joined {
                assert_eq!(response["usage"]["inflight_joins"].as_u64(), Some(1));
                assert_eq!(response["usage"]["source_bytes_read"].as_u64(), Some(0));
            }
            joined
        };
        let cached = |fixture: &Fixture| {
            joined(fixture) && fixture.store.lock_state().latest.contains_key(&identity)
        };
        let exact = exact_headroom(&build, &joined);
        let with_cache = exact_headroom(&build, &cached);
        let skipped = build();
        let (growth, reserved) = {
            let state = skipped.store.lock_state();
            (state.latest.growth(1), state.latest.reserved())
        };
        assert!(growth > 0);
        assert!(
            exact < with_cache && with_cache <= exact + growth,
            "the cache write costs its table growth: exact={exact} with_cache={with_cache} growth={growth}"
        );
        {
            let _filler = fill_to(&skipped.store, &skipped.owner, with_cache - 1);
            assert!(joined(&skipped), "the request itself was refused");
            skipped.store.assert_conserved();
            assert!(audited(&skipped.store)[TOTAL] <= cap_for(&skipped.owner));
            let state = skipped.store.lock_state();
            assert!(
                !state.latest.contains_key(&identity),
                "the refused cache write landed"
            );
            assert_eq!(
                state.latest.reserved(),
                reserved,
                "the refused cache write grew its table"
            );
        }
        let written = build();
        let _filler = fill_to(&written.store, &written.owner, with_cache);
        assert!(cached(&written));
        written.store.assert_conserved();
        assert!(audited(&written.store)[TOTAL] <= cap_for(&written.owner));
        assert!(written.store.lock_state().latest.reserved() > reserved);
    }
}

fn padded_warm_root_fixture(source: &LedgerSource) -> Fixture {
    let fixture = warm_root_fixture(source, true);
    let slot = parked_slot(&fixture);
    let mut state = fixture.store.lock_state();
    state.prepared_loads.remove(&slot.stamp.identity);
    for inode in 1u64.. {
        let padded = state.prepared_loads.len() == state.prepared_loads.reserved()
            && state.prepared_loads.len() >= 112;
        if padded {
            break;
        }
        state.insert_prepared_load(
            SourceIdentity {
                device: 0,
                inode,
                window_base: 0,
            },
            Arc::clone(&slot),
            now_ms(),
        );
    }
    drop(state);
    fixture
}

#[test]
fn root_warming_skips_the_prepared_load_pin_when_only_its_growth_does_not_fit() {
    let source = LedgerSource::new(&lines(0..8));
    let identity = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity;
    let build = || padded_warm_root_fixture(&source);
    let warmed = |fixture: &Fixture| admitted(&submit(fixture), "ok");
    let pinned = |fixture: &Fixture| {
        warmed(fixture)
            && fixture
                .store
                .lock_state()
                .prepared_loads
                .contains_key(&identity)
    };
    let exact = exact_headroom(&build, &warmed);
    let with_pin = exact_headroom(&build, &pinned);
    let skipped = build();
    let (growth, reserved) = {
        let state = skipped.store.lock_state();
        (
            state.prepared_loads.growth(1) + state.prepared_loads_expiry.growth(1),
            state.prepared_loads.reserved(),
        )
    };
    assert!(growth > 0);
    assert!(
        exact < with_pin && with_pin <= exact + growth,
        "the pin costs its growth: exact={exact} with_pin={with_pin} growth={growth}"
    );
    {
        let _filler = fill_to(&skipped.store, &skipped.owner, with_pin - 1);
        assert!(warmed(&skipped), "the request itself was refused");
        skipped.store.assert_conserved();
        assert!(audited(&skipped.store)[TOTAL] <= cap_for(&skipped.owner));
        let state = skipped.store.lock_state();
        assert!(
            !state.prepared_loads.contains_key(&identity),
            "the refused pin landed"
        );
        assert_eq!(
            state.prepared_loads.reserved(),
            reserved,
            "the refused pin grew its table"
        );
        assert!(
            state.loads.contains_key(&identity),
            "the parked load itself is kept"
        );
    }
    let written = build();
    let _filler = fill_to(&written.store, &written.owner, with_pin);
    assert!(pinned(&written));
    written.store.assert_conserved();
    assert!(audited(&written.store)[TOTAL] <= cap_for(&written.owner));
    assert!(written.store.lock_state().prepared_loads.reserved() > reserved);
}

#[test]
fn resumed_root_warming_admits_its_waiter_exactly() {
    let source = LedgerSource::new(&lines(0..8));
    let build = || warm_root_fixture(&source, true);
    let attempt = |fixture: &Fixture| admitted(&submit(fixture), "ok");
    let exact = exact_headroom(&build, &attempt);
    assert!(exact > 0);
    let refused = build();
    assert_refused_at("resume_warm_root", &refused, &attempt, exact);
    assert!(refused.store.lock_state().waiters.is_empty());
    assert!(refused.store.lock_state().prepared_loads.is_empty());
    let fitted = build();
    assert_fitted_at("resume_warm_root", &fitted, &attempt, exact);
    assert!(fitted.store.lock_state().waiters.is_empty());
}

#[test]
fn recent_codex_cache_write_is_skipped_when_only_its_growth_does_not_fit() {
    let source = LedgerSource::new(&codex_lines());
    for background in [false, true] {
        let build = || Fixture {
            store: fast_store(),
            owner: context_for("codex", background),
            request: acquire(&source.path),
            pins: Vec::new(),
        };
        let fitted = build();
        let growth = {
            let state = fitted.store.lock_state();
            state.recent_codex.growth(1) + state.recent_codex_expiry.growth(1)
        };
        assert!(growth > 0);
        let identity = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity;
        let (data, _pinned) = codex_acquired(&fitted);
        assert!(fitted.store.lock_state().latest.contains_key(&identity));
        {
            let _filler = fill_to(&fitted.store, &fitted.owner, 0);
            assert!(!finished_recent_codex(&fitted, &data));
            fitted.store.assert_conserved();
            assert!(audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner));
            let state = fitted.store.lock_state();
            assert!(!state.latest.contains_key(&identity));
            assert_eq!(state.recent_codex.reserved(), 0);
            assert_eq!(state.recent_codex_expiry.reserved(), 0);
            assert_eq!(state.recent_codex_raw_bytes, 0);
        }
        let admitted = build();
        let (data, pinned) = codex_acquired(&admitted);
        let _filler = fill_to(&admitted.store, &admitted.owner, growth);
        assert!(finished_recent_codex(&admitted, &data));
        admitted.store.assert_conserved();
        assert!(audited(&admitted.store)[TOTAL] <= cap_for(&admitted.owner));
        let state = admitted.store.lock_state();
        assert!(state.latest.contains_key(&identity));
        assert_eq!(
            state.recent_codex_raw_bytes,
            pinned.codex_raw.as_ref().unwrap().len()
        );
    }
}

#[test]
fn prepared_query_dom_is_charged_in_full_and_counts_against_the_cap() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    let padding = 64 * 1024;
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let small = exact_headroom(&|| input_page_fixture(&scenario, background, 0), &attempt);
        let large = exact_headroom(
            &|| input_page_fixture(&scenario, background, padding),
            &attempt,
        );
        assert!(small > REPLY_RESERVATION);
        assert_eq!(large - small, padding);
        let retained = |bytes: usize| {
            let fixture = input_page_fixture(&scenario, background, bytes);
            let before = settled(&fixture.store)[TOTAL];
            assert!(attempt(&fixture));
            fixture.store.assert_conserved();
            settled(&fixture.store)[TOTAL] - before
        };
        assert_eq!(retained(padding) - retained(0), padding);
        assert_boundary_at(
            "large prepared query",
            &input_page_fixture(&scenario, background, padding),
            &attempt,
            large,
        );
    }
}

fn pending_page_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let store = slow_store();
    let owner = context_for("pending", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    Fixture {
        store,
        owner,
        request: graph_query(
            &graph,
            json!({"kind":"has_read","pattern":format!("missing-{}", "x".repeat(padding)),"subagents":true}),
            json!([]),
        ),
        pins: vec![root_snapshot],
    }
}

#[test]
fn refused_prepared_query_page_releases_its_pending_source_waiter() {
    let scenario = Scenario::new(1, |_| lines(0..24));
    let padding = 64 * 1024;
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let build = |padding: usize| pending_page_fixture(&scenario, background, padding);
        let small = exact_headroom(&|| build(0), &attempt);
        let large = exact_headroom(&|| build(padding), &attempt);
        assert_eq!(
            large - small,
            padding,
            "the parked page is bound by its own cursor admission"
        );
        let refused = build(padding);
        let cap = cap_for(&refused.owner);
        {
            let _filler = fill_to(&refused.store, &refused.owner, large - 1);
            for _ in 0..3 {
                let copies = refused.store.warm_copies.load(Ordering::Relaxed);
                assert!(
                    !attempt(&refused),
                    "one byte over the exact fit parked the page"
                );
                assert!(
                    refused.store.warm_copies.load(Ordering::Relaxed) > copies,
                    "the refused page never parked a source"
                );
                refused.store.assert_conserved();
                assert!(audited(&refused.store)[TOTAL] <= cap);
                assert!(
                    refused.store.lock_state().waiters.is_empty(),
                    "the refused page left its pending source waiter behind"
                );
            }
        }
        let page = build(padding);
        let _filler = fill_to(&page.store, &page.owner, large);
        let response = submit(&page);
        assert!(parked(&response));
        let cursor = response["cursor"].as_str().unwrap();
        {
            let state = page.store.lock_state();
            let pending = state
                .prepared_queries
                .get(cursor)
                .expect("the parked page")
                .pending
                .as_ref()
                .expect("the parked page carries its pending source");
            assert_eq!(state.waiters.len(), 1);
            assert!(state.waiters.contains_key(&pending.token));
        }
        page.store.assert_conserved();
        assert!(audited(&page.store)[TOTAL] <= cap);
        release_cursor(&page.store, cursor, &page.owner);
        assert!(page.store.lock_state().waiters.is_empty());
        page.store.assert_conserved();
    }
}

fn restricted_context(claimant: &str, background: bool, root: &Path, padding: usize) -> Value {
    let mut context = context_for(claimant, background);
    let root = std::fs::canonicalize(root).unwrap();
    let roots: Vec<Value> = std::iter::once(json!(root.to_string_lossy().as_ref()))
        .chain((0..padding).map(|_| json!("r".repeat(4096))))
        .collect();
    context.insert(
        "authority",
        json!({"kind":"restricted_roots","effective_uid":unsafe { libc::geteuid() }.to_string(),"roots":roots}),
    );
    context
}

fn facts_cache_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let store = prepared_store();
    let owner = restricted_context("facts", background, &scenario.root.directory, padding);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let (_, sidechain_snapshot) = acquired(&store, &scenario.sidechains[0], &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    Fixture {
        store,
        owner,
        request: graph_query(&graph, missing_tool(), json!([])),
        pins: vec![root_snapshot, sidechain_snapshot],
    }
}

#[test]
fn prepared_facts_cache_admits_its_authority_metadata_exactly() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let identity = SourceStamp::of(&std::fs::metadata(&scenario.sidechains[0]).unwrap()).identity;
    let facts_cached = |fixture: &Fixture| {
        fixture
            .store
            .lock_state()
            .prepared_facts
            .contains_key(&identity)
    };
    let cached = |fixture: &Fixture| {
        admitted(
            &drive(&fixture.store, submit(fixture), &fixture.owner),
            "ok",
        ) && facts_cached(fixture)
    };
    let authority = |padding: usize| {
        value_bytes(
            &restricted_context("facts", false, &scenario.root.directory, padding)["authority"],
        )
    };
    for background in [false, true] {
        let build = |padding: usize| facts_cache_fixture(&scenario, background, padding);
        let small = exact_headroom(&|| build(16), &cached);
        let large = exact_headroom(&|| build(63), &cached);
        assert_eq!(
            large - small,
            authority(63) - authority(16),
            "the cache write is admitted with its authority metadata"
        );
        let retained = |padding: usize| {
            let fixture = build(padding);
            let before = settled(&fixture.store)[TOTAL];
            assert!(cached(&fixture));
            settled(&fixture.store)[TOTAL] - before
        };
        assert_eq!(retained(63) - retained(16), large - small);
        let skipped = build(63);
        let cap = cap_for(&skipped.owner);
        {
            let _filler = fill_to(&skipped.store, &skipped.owner, large - 1);
            let reply = drive(&skipped.store, submit(&skipped), &skipped.owner);
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert!(!facts_cached(&skipped), "the refused cache write landed");
            skipped.store.assert_conserved();
            assert!(audited(&skipped.store)[TOTAL] <= cap);
        }
        let written = build(63);
        let _filler = fill_to(&written.store, &written.owner, large);
        assert!(cached(&written));
        written.store.assert_conserved();
        assert!(audited(&written.store)[TOTAL] <= cap);
    }
}

#[test]
fn native_chunk_charge_covers_its_rows_header() {
    let source = LedgerSource::new(&lines(0..3));
    let store = fast_store();
    let owner = context_for("chunks", false);
    let (_handle, snapshot) = acquired(&store, &source.path, &owner);
    let chunk = &snapshot.chunks[0];
    let entries: usize = chunk
        .entry_charges
        .iter()
        .map(|charge| charge.owned_capacity_bytes)
        .sum();
    assert!(size_of::<ChunkRows>() > size_of::<Vec<Entry>>());
    assert_eq!(
        chunk.charge.owned_capacity_bytes,
        size_of::<EntryChunk>()
            + size_of::<ChunkRows>()
            + chunk.entries.capacity() * size_of::<Entry>()
            + chunk.entry_charges.capacity() * size_of::<MemoryCharge>()
            + entries
    );
}

#[test]
fn exact_fill_refuses_one_more_byte_and_reuses_released_capacity() {
    let sources: Vec<_> = (0..6)
        .map(|index| LedgerSource::new(&lines(index * 3..index * 3 + 3)))
        .collect();
    for background in [false, true] {
        let store = fast_store();
        let owner = context_for("capacity", background);
        let retained: Vec<_> = sources
            .iter()
            .map(|source| acquired(&store, &source.path, &owner))
            .collect();
        let rows: Vec<_> = retained
            .iter()
            .map(|(_, snapshot)| Arc::clone(&snapshot.chunks[0].entries))
            .collect();
        let baseline = settled(&store);
        store.assert_conserved();
        assert_eq!(baseline[GENERATIONS], sources.len());
        assert_eq!(baseline[LEASES], sources.len());
        let cap = cap_for(&owner);
        for _ in 0..2 {
            let fill = store
                .reserve_projection(&owner, cap - baseline[TOTAL])
                .unwrap();
            let filled = ledger(&store);
            assert_eq!(filled[TOTAL], cap);
            assert_eq!(audited(&store)[TOTAL], cap);
            store.assert_conserved();
            let refused = store.reserve_projection(&owner, 1);
            assert!(matches!(refused, Err(error) if error.status == Status::RetainedLimit));
            assert_eq!(ledger(&store), filled);
            store.assert_conserved();
            drop(fill);
            assert_eq!(ledger(&store), baseline);
            store.assert_conserved();
        }
        drop(rows);
        drop(retained);
        store.assert_conserved();
    }
}

#[test]
fn pressure_eviction_frees_unleased_latest_within_the_cap() {
    let sources: Vec<_> = (0..6)
        .map(|index| LedgerSource::new(&lines(index * 3..index * 3 + 3)))
        .collect();
    for background in [false, true] {
        let store = fast_store();
        let owner = context_for("pressure", background);
        let mut retained: Vec<_> = sources
            .iter()
            .map(|source| acquired(&store, &source.path, &owner))
            .collect();
        for (handle, snapshot) in retained.split_off(2) {
            release_lease(&store, &handle, &owner);
            drop(snapshot);
        }
        let before = settled(&store);
        store.assert_conserved();
        assert_eq!(before[GENERATIONS], sources.len());
        assert_eq!(before[LEASES], 2);
        let cap = cap_for(&owner);
        let pressure = store
            .reserve_projection(&owner, cap - before[TOTAL] + 1)
            .unwrap();
        let after = ledger(&store);
        store.assert_conserved();
        assert_eq!(after[GENERATIONS], 2);
        assert!(after[TOTAL] <= cap);
        assert!(audited(&store)[TOTAL] <= cap);
        let kept: Vec<_> = retained.iter().map(|(_, snapshot)| snapshot).collect();
        assert_eq!(after[ENTRIES], entry_bytes(&kept));
        drop(pressure);
        store.assert_conserved();
        assert_eq!(
            ledger(&store)[TOTAL],
            after[TOTAL] - (cap - before[TOTAL] + 1)
        );
    }
}

#[test]
fn last_owners_dropped_under_the_state_lock_queue_without_deadlock() {
    let source = LedgerSource::new(&line("locked"));
    let store = Arc::new(fast_store());
    let owner = context_for("locked", false);
    let (orphan, replacement) = orphaned(&store, &source, &owner);
    let rows = Arc::clone(&orphan.chunks[0].entries);
    let (sender, receiver) = mpsc::channel();
    let worker_store = Arc::clone(&store);
    let worker = std::thread::spawn(move || {
        let state = worker_store.lock_state();
        drop(orphan);
        let alone = Arc::strong_count(&rows);
        drop(rows);
        drop(state);
        sender.send(alone).unwrap();
    });
    let alone = receiver
        .recv_timeout(DROP_TIMEOUT)
        .expect("dropping the last owners under the state lock deadlocked");
    worker.join().unwrap();
    assert_eq!(alone, 1);
    store.assert_conserved();
    let released = ledger(&store);
    assert_eq!(released[GENERATIONS], 1);
    assert_eq!(released[ENTRIES], entry_bytes(&[&replacement]));
}

#[test]
fn pressure_eviction_drops_the_last_snapshot_under_the_state_lock() {
    let source = LedgerSource::new(&lines(0..2));
    for background in [false, true] {
        let store = Arc::new(fast_store());
        let owner = context_for("evicted", background);
        let (handle, snapshot) = acquired(&store, &source.path, &owner);
        release_lease(&store, &handle, &owner);
        drop(snapshot);
        assert_eq!(settled(&store)[GENERATIONS], 1);
        let (sender, receiver) = mpsc::channel();
        let worker_store = Arc::clone(&store);
        let worker_owner = owner.clone();
        let worker = std::thread::spawn(move || {
            evict_unpinned(&worker_store, &worker_owner);
            sender.send(()).unwrap();
        });
        receiver
            .recv_timeout(DROP_TIMEOUT)
            .expect("evicting the last snapshot under the state lock deadlocked");
        worker.join().unwrap();
        store.assert_conserved();
        let evicted = ledger(&store);
        assert_eq!(evicted[GENERATIONS], 0);
        assert_eq!(evicted[ENTRIES], 0);
    }
}

#[test]
fn last_owners_dropped_on_another_thread_do_not_wait_for_the_state_lock() {
    let source = LedgerSource::new(&line("crossed"));
    let store = fast_store();
    let owner = context_for("crossed", false);
    let (orphan, replacement) = orphaned(&store, &source, &owner);
    let rows = Arc::clone(&orphan.chunks[0].entries);
    let (sender, receiver) = mpsc::channel();
    let guard = store.lock_state();
    let worker = std::thread::spawn(move || {
        drop(orphan);
        drop(rows);
        sender.send(()).unwrap();
    });
    let dropped = receiver.recv_timeout(DROP_TIMEOUT);
    drop(guard);
    dropped.expect("a dropped owner waited for the state lock");
    worker.join().unwrap();
    store.assert_conserved();
    let released = ledger(&store);
    assert_eq!(released[GENERATIONS], 1);
    assert_eq!(released[ENTRIES], entry_bytes(&[&replacement]));
}

#[test]
fn concurrent_acquire_pin_release_and_drop_churn_stays_conserved() {
    let sources: Vec<_> = (0..3)
        .map(|index| Arc::new(LedgerSource::new(&lines(index * 4..index * 4 + 4))))
        .collect();
    let store = Arc::new(store_with(256, 4, &[]));
    let (done, finished) = mpsc::channel();
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let store = Arc::clone(&store);
            let sources = sources.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let owner = context_for(&format!("churn-{worker}"), worker % 2 == 1);
                let mut held = Vec::new();
                for round in 0..16 {
                    let source = &sources[(worker + round) % sources.len()];
                    if worker == 0 && round % 4 == 0 {
                        source.append(&line(&format!("churn-{round}")));
                    }
                    let response = settle(
                        &store,
                        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                        &owner,
                    );
                    let handle = ok_handle(&response);
                    let snapshot = store.pin(&handle, &owner).unwrap();
                    held.push(Arc::clone(&snapshot.chunks[0].entries));
                    release_lease(&store, &handle, &owner);
                    drop(snapshot);
                    if round % 3 == 0 {
                        held.clear();
                    }
                    if round % 4 == worker {
                        evict_unpinned(&store, &owner);
                    }
                }
                drop(held);
                done.send(worker).unwrap();
            })
        })
        .collect();
    drop(done);
    for _ in 0..workers.len() {
        finished
            .recv_timeout(CHURN_TIMEOUT)
            .expect("a churn worker never finished");
    }
    for worker in workers {
        worker.join().unwrap();
    }
    store.assert_conserved();
    evict_unpinned(&store, &context_for("churn-final", false));
    store.assert_conserved();
    let quiesced = ledger(&store);
    assert_eq!(quiesced[GENERATIONS], 0);
    assert_eq!(quiesced[ENTRIES], 0);
    assert_eq!(quiesced[LEASES], 0);
}

#[test]
fn concurrent_root_slice_queries_stay_conserved() {
    let scenario = Scenario::new(0, |_| String::new());
    let store = Arc::new(prepared_store());
    let owner = context_for("racing-slices", false);
    let (root, _root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &[], &owner);
    let query = graph_query(
        &graph,
        json!({"kind":"has_read","pattern":"missing","subagents":true}),
        json!([{"kind":"current_turn"}]),
    );
    let (done, finished) = mpsc::channel();
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let store = Arc::clone(&store);
            let owner = owner.clone();
            let query = query.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                for _ in 0..8 {
                    let reply = store.request(&query, &owner, &Cancellation::default());
                    assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
                    assert_eq!(reply["data"]["value"].as_bool(), Some(false));
                }
                done.send(worker).unwrap();
            })
        })
        .collect();
    drop(done);
    for _ in 0..workers.len() {
        finished
            .recv_timeout(CHURN_TIMEOUT)
            .expect("a slice worker never finished");
    }
    for worker in workers {
        worker.join().unwrap();
    }
    store.assert_conserved();
    release_graph(&store, &graph, &owner);
    store.assert_conserved();
}

#[test]
fn f2_prepared_sidechain_generations_stay_charged_until_pressure_evicts_them() {
    let scenario = Scenario::new(8, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let store = prepared_store();
        let owner = context_for("retained", background);
        let (_, root) = acquired(&store, &scenario.root.path, &owner);
        warm(&store, &scenario, &owner);
        store.assert_conserved();
        assert_eq!(settled(&store)[GENERATIONS], scenario.sidechains.len() + 1);
        evict_unpinned(&store, &owner);
        store.assert_conserved();
        let evicted = ledger(&store);
        assert_eq!(evicted[GENERATIONS], 1);
        assert_eq!(evicted[ENTRIES], entry_bytes(&[&root]));
    }
}

#[test]
fn shared_warm_buffers_are_charged_once_across_their_owners() {
    let scenario = scaling_scenario(8);
    let warmed = warmed_registry(prepared_store(), &scenario);
    let (store, owner) = (&warmed.store, &warmed.owner);
    store.assert_conserved();
    let first_id = warmed.graph["graph_id"].as_str().unwrap().to_owned();
    let (membership_key, membership_charge, shared, first_charge) = {
        let state = store.lock_state();
        let (key, membership) = state
            .warm_memberships
            .iter()
            .next()
            .expect("warmed membership");
        let first = state.prepared_graphs[&first_id].lock().unwrap();
        assert!(Arc::ptr_eq(&first.sources, &membership.members));
        assert!(Arc::ptr_eq(
            &first.sidechain_dirs,
            &membership.sidechain_dirs
        ));
        (
            key.clone(),
            charged_bytes(key, membership),
            source_ref_bytes(&membership.members) + sidechain_dir_bytes(&membership.sidechain_dirs),
            charged_bytes(&first_id, &*first),
        )
    };
    assert!(shared > 0);
    assert_eq!(warm_buffer_charge(store), shared);
    let one_graph = settled_charges(store);
    let second = settle(
        store,
        store.request(
            &prepare_request(&warmed.root, &scenario.ids(), &scenario.roots(), &[]),
            owner,
            &Cancellation::default(),
        ),
        owner,
    );
    assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
    let second_id = second["data"]["handle"]["graph_id"]
        .as_str()
        .unwrap()
        .to_owned();
    store.assert_conserved();
    let second_charge = {
        let state = store.lock_state();
        charged_bytes(&second_id, &state.prepared_graphs[&second_id])
    };
    let two_graphs = settled_charges(store);
    eprintln!(
        "shared warm buffers: shared={shared} graph={second_charge} one_graph={one_graph} two_graphs={two_graphs}"
    );
    assert_eq!(two_graphs - one_graph, second_charge);
    assert_eq!(warm_buffer_charge(store), shared);
    store.lock_state().remove_prepared_graph(&first_id);
    store.assert_conserved();
    assert_eq!(settled_charges(store), two_graphs - first_charge);
    assert_eq!(warm_buffer_charge(store), shared);
    store.lock_state().remove_warm_membership(&membership_key);
    store.assert_conserved();
    assert_eq!(
        settled_charges(store),
        two_graphs - first_charge - membership_charge
    );
    assert_eq!(warm_buffer_charge(store), shared);
    store.lock_state().remove_prepared_graph(&second_id);
    store.assert_conserved();
    assert_eq!(
        settled_charges(store),
        two_graphs - first_charge - membership_charge - second_charge - shared
    );
    assert_eq!(warm_buffer_charge(store), 0);
}

#[test]
fn warm_anchor_churn_leaves_tombstones_without_moving_the_shared_charge() {
    let scenario = scaling_scenario(8);
    let warmed = warmed_registry(prepared_store(), &scenario);
    let store = &warmed.store;
    let churned = {
        let state = store.lock_state();
        let membership = state
            .warm_memberships
            .values()
            .next()
            .expect("warmed membership");
        WarmMembership {
            members: membership.members.iter().cloned().collect(),
            sidechain_dirs: membership.sidechain_dirs.iter().cloned().collect(),
            ..membership.clone()
        }
    };
    let mut next = 1usize;
    let mut live = Vec::new();
    let mut dipped = false;
    let mut rounds = 0;
    while !dipped && rounds < 256 {
        let table = {
            let mut state = store.lock_state();
            if !live.is_empty() {
                state.ledger.shared.release(live.remove(0));
            }
            while state.ledger.shared.len() + 2 < state.ledger.shared.reserved() {
                state.ledger.shared.acquire(Anchor::facts(next, 0));
                live.push(next);
                next += 1;
            }
            state.ledger.shared.work().traced();
            state.ledger.shared.table_bytes()
        };
        let charged = settled_charges(store);
        store
            .lock_state()
            .insert_warm_membership("churn".to_owned(), churned.clone());
        let inserted = {
            let state = store.lock_state();
            let traced = state.ledger.shared.work().traced();
            let inserted = state.ledger.shared.table_bytes();
            if inserted == table {
                assert!(traced.is_empty(), "allocation without a charge change");
            } else {
                assert_eq!(
                    traced,
                    vec![Trace::Allocated(state.ledger.shared.reserved())]
                );
            }
            inserted
        };
        store.assert_conserved();
        store.lock_state().remove_warm_membership("churn");
        {
            let state = store.lock_state();
            assert_eq!(
                state.ledger.shared.table_bytes(),
                inserted,
                "release moved the charge"
            );
            dipped |= state.ledger.shared.capacity() < state.ledger.shared.reserved();
        }
        store.assert_conserved();
        assert_eq!(settled_charges(store), charged);
        rounds += 1;
    }
    assert!(dipped, "no erase left a tombstone in {rounds} rounds");
}

#[test]
fn f2_cached_single_source_hits_cost_constant_work_for_every_retained_count() {
    let mut every_hit = Vec::new();
    for count in SCALING_COUNTS {
        let scenario = scaling_scenario(count);
        let warmed = warmed_registry(prepared_store(), &scenario);
        let (store, owner) = (&warmed.store, &warmed.owner);
        assert!(
            ledger(store)[GENERATIONS] > count,
            "F2 must retain every prepared sidechain at {count}"
        );
        store.assert_conserved();
        let path = &scenario.sidechains[count / 2];
        let audits = store.audits.load(Ordering::Relaxed);
        let mut hits = Vec::new();
        for hit in 0..8 {
            let hit = cached_hit(store, path, owner, &format!("hit {hit} at {count}"));
            assert!(
                hit.work <= WARM_HIT_BOUND,
                "a hit at {count} did {} units of retained work",
                hit.work
            );
            assert_eq!(hit.copies, 0, "a hit at {count} copied retained elements");
            assert_eq!(hit.reclaims, 0, "a hit at {count} drained retained state");
            hits.push(hit.work);
        }
        eprintln!("cached hits: count={count} retained_work={hits:?}");
        assert_eq!(store.audits.load(Ordering::Relaxed), audits);
        store.assert_conserved();
        every_hit.extend(hits);
    }
    assert_steady("cached hits across every retained count", &every_hit);
}

#[test]
fn f2_a_thousand_cached_hits_do_the_same_retained_work() {
    let count = 923;
    let scenario = scaling_scenario(count);
    let warmed = warmed_registry(
        store_with(4096, 2048, &[("max_lease_ms", 600_000)]),
        &scenario,
    );
    let (store, owner) = (&warmed.store, &warmed.owner);
    let path = &scenario.sidechains[count / 2];
    let mut steady = Vec::new();
    let mut draining = Vec::new();
    for hit in 0..STEADY_HITS {
        let measured = cached_hit(store, path, owner, &format!("hit {hit}"));
        assert!(
            measured.work <= WARM_HIT_BOUND,
            "hit {hit} did {} units of retained work",
            measured.work
        );
        assert_eq!(measured.copies, 0, "hit {hit} copied retained elements");
        if measured.reclaims > 0 {
            draining.push((hit, measured));
        } else {
            steady.push(measured.work);
        }
    }
    eprintln!(
        "thousand cached hits: steady={} draining={draining:?} min={:?} max={:?}",
        steady.len(),
        steady.iter().min(),
        steady.iter().max()
    );
    assert!(
        draining.is_empty(),
        "cached hits drained retained state: {draining:?}"
    );
    assert_steady("a thousand cached hits", &steady);
    store.assert_conserved();
}

#[test]
fn f2_graph_pages_cost_bounded_work_per_page_request() {
    for count in SCALING_COUNTS {
        let scenario = scaling_scenario(count);
        let warmed = warmed_registry(prepared_store(), &scenario);
        let (store, owner, graph) = (&warmed.store, &warmed.owner, &warmed.graph);
        assert!(
            ledger(store)[GENERATIONS] > count,
            "F2 must retain every prepared sidechain at {count}"
        );
        let audits = store.audits.load(Ordering::Relaxed);
        let mut markers = Vec::new();
        for query in 0..4 {
            let Measured { work, copies, .. } = measured(store, || {
                let reply = settle(
                    store,
                    store.request(
                        &graph_query(graph, marker_tool(), json!([])),
                        owner,
                        &Cancellation::default(),
                    ),
                    owner,
                );
                assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
                assert_eq!(reply["data"]["value"].as_bool(), Some(true));
            });
            assert!(
                work <= WARM_HIT_BOUND,
                "marker query {query} at {count} did {work} units of retained work"
            );
            assert!(
                copies <= WARM_HIT_BOUND,
                "marker query {query} at {count} copied {copies} retained elements"
            );
            markers.push((work, copies));
        }
        eprintln!("marker queries: count={count} retained_work_and_warm_copies={markers:?}");
        let (reply, pages) =
            paged_costs(store, graph_query(graph, missing_tool(), json!([])), owner);
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(reply["data"]["value"].as_bool(), Some(false));
        eprintln!(
            "graph pages: count={count} page_requests={} retained_work_and_warm_copies={pages:?}",
            pages.len()
        );
        assert_page_costs("a negative pass", count, &pages);
        assert_eq!(store.audits.load(Ordering::Relaxed), audits);
        store.assert_conserved();
    }
}

#[test]
fn f2_facts_over_budget_pages_cost_bounded_work_per_page_request() {
    for count in [100, 500] {
        let scenario = scaling_scenario(count);
        let warmed = warmed_registry(
            store_with(4096, 2048, &[("max_prepared_fact_memory_bytes", 16 * 1024)]),
            &scenario,
        );
        let (store, owner, graph) = (&warmed.store, &warmed.owner, &warmed.graph);
        assert!(
            ledger(store)[GENERATIONS] > count,
            "F2 must retain every prepared sidechain at {count}"
        );
        let (reply, pages) =
            paged_costs(store, graph_query(graph, missing_tool(), json!([])), owner);
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(reply["data"]["value"].as_bool(), Some(false));
        eprintln!(
            "facts over budget: count={count} page_requests={} retained_work_and_warm_copies={pages:?}",
            pages.len()
        );
        assert_page_costs("evicting facts", count, &pages);
        store.assert_conserved();
    }
}

#[test]
#[ignore]
fn ledger_scaling() {
    for count in SCALING_COUNTS {
        let scenario = scaling_scenario(count);
        let warmed = warmed_registry(prepared_store(), &scenario);
        let path = &scenario.sidechains[count / 2];
        for _ in 0..16 {
            warm_hit(&warmed.store, path, &warmed.owner);
        }
        let hits = 512;
        let started = Instant::now();
        for _ in 0..hits {
            warm_hit(&warmed.store, path, &warmed.owner);
        }
        let elapsed = started.elapsed();
        let live = stats_gauges(&warmed.store, &warmed.owner)["live_generations"]
            .as_u64()
            .unwrap();
        eprintln!(
            "ledger_scaling count={count} live_generations={live} warm_hit_us={:.2}",
            elapsed.as_secs_f64() * 1e6 / hits as f64
        );
    }
}

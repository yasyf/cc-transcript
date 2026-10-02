use super::*;
use crate::activity::lower_edit;
use crate::snapshot_activity::{CachedCall, CachedTurn, ResultPosition};
use crate::snapshot_ledger::Trace;
use crate::snapshot_prepared::{OverrideEvent, PreparedFacts};
use crate::snapshot_prepared_disk::PreparedDiskKey;
use crate::toolcall::{parse_tool_call, ToolCall};
use crate::types::{joined_text, ContentBlock};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CAP: usize = 32 * 1024 * 1024;
const HOOK_BYTES: usize = 4096;
const REPLY_RESERVATION: usize = 2 * MAX_REPLY_BYTES;
const SEARCH_LIMIT: usize = 4 * 1024 * 1024;
const SLOW_READ_STEP: usize = 64;
const SEEDED_EVENTS: usize = 32;
const SEEDED_STEP: usize = 2;
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
const BUILDS: usize = 0;
const LOOKUPS: usize = 1;
const RECORDS: usize = 0;
const SOURCES: usize = 1;
const NO_LOADS: LoadResidue = LoadResidue {
    slots: 0,
    pinned: 0,
};
const UNPINNED_LOAD: LoadResidue = LoadResidue {
    slots: 1,
    pinned: 0,
};
const PINNED_LOAD: LoadResidue = LoadResidue {
    slots: 1,
    pinned: 1,
};

struct LedgerSource {
    directory: PathBuf,
    path: PathBuf,
}

impl LedgerSource {
    fn new(contents: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "cc-ledger-{}-{}-{:06}",
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

#[derive(Debug, Clone, Copy)]
struct LoadResidue {
    slots: usize,
    pinned: usize,
}

struct Replacement {
    response: Value,
    held: Arc<PreparedFacts>,
    fresh: Arc<PreparedFacts>,
    counted: usize,
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

fn cap_of(store: &NativeStore, context: &Value) -> usize {
    if context["work_class"].as_str() == Some("background") {
        store.config.retained - store.config.hook_bytes
    } else {
        store.config.retained
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

fn warm_with(store: &NativeStore, mut request: Value, rounds: usize, owner: &Value) {
    let mut background = owner.clone();
    background.insert("work_class", json!("background"));
    for _ in 0..rounds {
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

fn warm(store: &NativeStore, scenario: &Scenario, owner: &Value) {
    warm_with(
        store,
        warm_request(scenario),
        scenario.sidechains.len() + 16,
        owner,
    );
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
    let cap = cap_of(store, owner);
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

fn exact_fit(fits: &dyn Fn(usize) -> bool) -> usize {
    assert!(
        fits(SEARCH_LIMIT),
        "nothing was admitted within {SEARCH_LIMIT} bytes of headroom"
    );
    if fits(0) {
        return 0;
    }
    let (mut refused, mut fitted) = (0, SEARCH_LIMIT);
    while fitted - refused > 1 {
        let middle = refused + (fitted - refused) / 2;
        if fits(middle) {
            fitted = middle;
        } else {
            refused = middle;
        }
    }
    fitted
}

fn exact_headroom(build: &dyn Fn() -> Fixture, attempt: &dyn Fn(&Fixture) -> bool) -> usize {
    exact_fit(&|headroom| {
        let fixture = build();
        let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
        attempt(&fixture)
    })
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
        arc_slice_mirror::<PreparedSourceRef>(graph.sources.len())
            + graph
                .sources
                .iter()
                .map(|source| source.path.capacity())
                .sum::<usize>()
    };
    let dirs = if shared_dirs {
        0
    } else {
        arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(graph.sidechain_dirs.len())
            + graph
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
    };
    key + arc_mirror::<Mutex<PreparedGraph>>()
        + MUTEX_STORAGE_MIRROR
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
            .map(|(path, _)| path.capacity())
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

fn published_graph_allocations(state: &StoreState, graph_id: &str, graph: &PreparedGraph) -> usize {
    state
        .prepared_graphs
        .iter()
        .find(|(key, _)| key.as_str() == graph_id)
        .map(|(key, _)| key.capacity())
        .expect("published graph key")
        + arc_mirror::<Mutex<PreparedGraph>>()
        + MUTEX_STORAGE_MIRROR
        + graph.registry_generation.capacity()
        + graph.admission.capacity()
        + graph.revision.capacity()
        + value_bytes(&graph.authority)
        + arc_slice_mirror::<PreparedSourceRef>(graph.sources.len())
        + arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(graph.sidechain_dirs.len())
}

fn moved_build_bytes(graph: &PreparedGraph) -> usize {
    graph.claimant.capacity()
        + value_bytes(&graph.root_handle)
        + value_bytes(&graph.classifier)
        + graph.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
        + graph
            .stamps
            .iter()
            .map(|(path, _)| path.capacity())
            .sum::<usize>()
        + graph
            .sources
            .iter()
            .map(|source| source.path.capacity())
            .sum::<usize>()
        + graph
            .sidechain_dirs
            .iter()
            .map(|(path, _)| path.capacity())
            .sum::<usize>()
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
        let allocated = published_graph_allocations(&state, &graph_id, &graph);
        reserved_before_the_last_admission("direct prepare_graph", &traced, allocated);
        let publication = traced
            .iter()
            .position(|trace| *trace == Trace::Reserved(allocated))
            .unwrap();
        assert!(
            reserved_traces(&traced[..publication]).iter().sum::<usize>()
                >= REPLY_RESERVATION + moved_build_bytes(&graph),
            "direct prepare_graph: buffers moved into the graph were not reserved before publication: {traced:?}"
        );
        assert_eq!(
            constructed_graph_bytes(&state, &graph_id, &graph),
            allocated + moved_build_bytes(&graph)
        );
    }
}

#[test]
fn root_facts_are_reserved_at_their_bound_before_parsing() {
    let scenario = Scenario::new(0, |_| String::new());
    for background in [false, true] {
        let store = prepared_store();
        let owner = context_for("facts", background);
        let (root, snapshot) = acquired(&store, &scenario.root.path, &owner);
        let bound = facts_bound_walk(&snapshot);
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
        reserved_before_the_last_admission("root facts", &traced(&store), bound);
        let cached = NativeStore::audit_facts_bytes(
            &store.lock_state().prepared_facts[&snapshot.stamp.identity].facts,
        );
        assert_eq!(
            cached, bound,
            "root facts: the cached {cached}-byte facts are not the {bound} bytes reserved"
        );
    }
}

#[test]
fn root_slices_are_reserved_at_the_root_bound_before_preparation() {
    let scenario = Scenario::new(0, |_| String::new());
    for background in [false, true] {
        let fixture = slice_fixture(&scenario, background);
        let graph_id = fixture.request["handle"]["graph_id"].as_str().unwrap();
        let bound = facts_bound_walk(&fixture.pins[0]);
        let existing = {
            let state = fixture.store.lock_state();
            let graph = state.prepared_graphs[graph_id].lock().unwrap();
            graph.root_slices.keys().cloned().collect::<Vec<_>>()
        };
        traced(&fixture.store);
        let response = submit(&fixture);
        assert!(admitted(&response, "ok"), "{response:?}");
        reserved_before_the_last_admission("root slice", &traced(&fixture.store), bound);
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[graph_id].lock().unwrap();
        let slice = graph
            .root_slices
            .iter()
            .find(|(key, _)| !existing.contains(*key))
            .map(|(_, facts)| NativeStore::audit_facts_bytes(facts))
            .expect("published root slice");
        assert!(
            bound >= slice,
            "root slice: the {bound}-byte root bound does not cover the {slice}-byte slice"
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
        let build = || classifier_stage_fixture(&source, &long, background);
        refused_at_site("classifier stage", &build, &stage_created, 0, large);
        let fixture = build();
        let before = fixture.store.lock_state().ledger.classifier;
        assert_fitted_at("classifier stage", &fixture, &stage_created, large);
        let state = fixture.store.lock_state();
        let (key, slot) = state
            .classifier_stages
            .iter()
            .next()
            .expect("created classifier stage");
        assert!(key.capacity() > 16 * 1024);
        assert_eq!(
            state.ledger.classifier - before,
            arc_mirror::<ClassifierSlot>()
                + MUTEX_STORAGE_MIRROR
                + key.capacity()
                + slot.accounted.load(Ordering::Acquire),
            "the classifier gauge misses the stage slot, its mutex storage, or its key"
        );
    }
}

fn seeded_classifier_fixture(source: &LedgerSource, classifier: &str, background: bool) -> Fixture {
    std::fs::write(&source.path, lines(0..SEEDED_EVENTS)).unwrap();
    let store = fast_store();
    let owner = context_for("seeded", background);
    recording_classifier(&store, classifier);
    let (_, seeded) = classified(&store, &source.path, classifier, &owner);
    assert_eq!(seeded.event_count, SEEDED_EVENTS);
    source.append(&lines(SEEDED_EVENTS..SEEDED_EVENTS + 2 * SEEDED_STEP));
    let (_, native) = acquired(&store, &source.path, &owner);
    assert_eq!(native.event_count, SEEDED_EVENTS + 2 * SEEDED_STEP);
    let request = json!({"id":classifier,"version":"1"});
    let created = store.classify(
        Arc::clone(&native),
        &request,
        &owner,
        &Cancellation::default(),
        &WorkLimits {
            max_events: 0,
            ..work_bounds()
        },
        &mut [0u64; 18],
    );
    assert!(matches!(created, Err(error) if error.status == Status::Incomplete));
    Fixture {
        store,
        owner,
        request,
        pins: vec![native],
    }
}

fn seeded_slot(fixture: &Fixture) -> Arc<ClassifierSlot> {
    let mut state = fixture.store.lock_state();
    NativeStore::prune(&mut state);
    let slots: Vec<_> = state.classifier_stages.values().cloned().collect();
    let [slot] = slots.as_slice() else {
        panic!(
            "expected one seeded classifier stage, found {}",
            slots.len()
        );
    };
    Arc::clone(slot)
}

fn seeded_step_parked(fixture: &Fixture) -> bool {
    let bounds = WorkLimits {
        max_events: SEEDED_STEP,
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
        Ok(progress) => {
            assert!(
                progress.snapshot.is_none() && progress.stage.is_some(),
                "the seeded step published past its event budget"
            );
            assert_eq!(progress.events, SEEDED_STEP);
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("seeded classifier step failed outside admission: {error:?}"),
    }
}

fn seeded_stage_walk(slot: &ClassifierSlot) -> usize {
    let stage = slot.work.lock().unwrap();
    let ClassifierStage {
        activity,
        indexed: _,
        carried,
        committed,
        result: _,
    } = &*stage;
    let seed: HashSet<usize> = slot
        .seed
        .as_ref()
        .expect("seeded stage")
        .activity()
        .audited_allocations(true)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut seen = HashSet::new();
    activity
        .audited_heap_allocations()
        .into_iter()
        .chain(
            committed
                .iter()
                .flat_map(ActivityIndex::audited_heap_allocations),
        )
        .filter(|(id, _)| !seed.contains(id) && seen.insert(*id))
        .map(|(_, bytes)| bytes)
        .sum::<usize>()
        + carried.capacity() * size_of::<usize>()
}

fn append_reservation_mirror(
    activity: &ActivityIndex,
    shared_with: &ActivityIndex,
    indexed: &[&Entry],
    entries: usize,
    calls: usize,
    results: usize,
) -> usize {
    let growth = |capacity: usize, needed: usize, width: usize| {
        if needed > capacity {
            (needed.max(capacity) * 2).max(8) * width
        } else {
            0
        }
    };
    let carried: HashSet<usize> = shared_with
        .audited_allocations(true)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let rows = activity.audited_allocations(false);
    let [_, (turns_id, turns_row), (uuids_id, uuids_row), (results_id, results_row), turn_rows @ ..] =
        rows.as_slice()
    else {
        panic!("an activity index walks its three containers first: {rows:?}");
    };
    assert_eq!(
        (turn_rows.len(), activity.calls().count(), calls),
        (activity.turn_count(), 0, 0),
        "the mirror covers call-free indexes and appends"
    );
    let uuid_keys: usize = indexed
        .iter()
        .filter_map(|entry| entry.meta())
        .map(|meta| meta.uuid.len())
        .sum();
    let uuids = indexed
        .iter()
        .filter(|entry| entry.meta().is_some())
        .count();
    let result_keys: usize = indexed
        .iter()
        .flat_map(|entry| entry.tool_results())
        .map(|result| result.tool_use_id.len())
        .sum();
    let indexed_results = indexed
        .iter()
        .flat_map(|entry| entry.tool_results())
        .count();
    let turns_capacity =
        (turns_row - arc_mirror::<Vec<Arc<CachedTurn>>>()) / size_of::<Arc<CachedTurn>>();
    let uuids_capacity = (uuids_row - arc_mirror::<HashMap<String, usize>>() - uuid_keys)
        / size_of::<(String, usize)>();
    let results_capacity =
        (results_row - arc_mirror::<HashMap<String, ResultPosition>>() - result_keys)
            / size_of::<(String, ResultPosition)>();
    let copied = |id: &usize, row: &usize| if carried.contains(id) { *row } else { 0 };
    let open_turn = turn_rows.last().map_or(0, |(id, row)| {
        if carried.contains(id) || carried.contains(turns_id) {
            *row
        } else {
            0
        }
    });
    copied(uuids_id, uuids_row)
        + copied(results_id, results_row)
        + copied(turns_id, turns_row)
        + open_turn
        + growth(
            uuids_capacity,
            uuids + entries,
            size_of::<(String, usize)>(),
        )
        + growth(
            results_capacity,
            indexed_results + results,
            size_of::<(String, ResultPosition)>(),
        )
        + growth(
            turns_capacity,
            activity.turn_count() + entries,
            size_of::<Arc<CachedTurn>>(),
        )
        + entries * arc_mirror::<CachedTurn>()
        + calls * (arc_mirror::<CachedCall>() + 4 * size_of::<Arc<CachedCall>>())
}

fn seeded_step_reserve(fixture: &Fixture) -> (usize, usize) {
    let native = &fixture.pins[0];
    let (bytes, calls, results) = (SEEDED_EVENTS..SEEDED_EVENTS + SEEDED_STEP)
        .map(|position| native.entry(position))
        .fold(
            (0usize, 0usize, 0usize),
            |(bytes, calls, results), entry| {
                let charge = entry_charge(entry);
                (
                    bytes + charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
                    calls + entry.tool_uses().count(),
                    results + entry.tool_results().count(),
                )
            },
        );
    let slot = seeded_slot(fixture);
    let stage = slot.work.lock().unwrap();
    assert_eq!(stage.indexed, SEEDED_EVENTS);
    let indexed: Vec<&Entry> = (0..SEEDED_EVENTS)
        .map(|position| native.entry(position))
        .collect();
    let reserve = 2 * bytes
        + SEEDED_STEP * (size_of::<&Entry>() + size_of::<bool>())
        + append_reservation_mirror(
            &stage.activity,
            slot.seed.as_ref().expect("seeded stage").activity(),
            &indexed,
            SEEDED_STEP,
            calls,
            results,
        );
    (reserve, 2 * bytes + SEEDED_STEP)
}

#[test]
fn seeded_classifier_step_admits_its_append_growth_before_appending() {
    let source = LedgerSource::new("");
    let site = "seeded classifier step";
    for background in [false, true] {
        let build = || seeded_classifier_fixture(&source, "seeded", background);
        let exact = exact_headroom(&build, &seeded_step_parked);
        let (reserve, old_reserve) = seeded_step_reserve(&build());
        assert_eq!(
            exact, reserve,
            "{site}: the step admission is not the input heuristic plus the container growth"
        );
        let refused = build();
        let slot = seeded_slot(&refused);
        let before = slot.accounted.load(Ordering::Acquire);
        assert_refused_at(site, &refused, &seeded_step_parked, exact);
        {
            let stage = slot.work.lock().unwrap();
            assert_eq!(stage.indexed, SEEDED_EVENTS, "{site}: the refusal appended");
            assert_eq!(stage.activity.entry_count(), SEEDED_EVENTS);
            assert!(stage.committed.is_none());
        }
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            before,
            "{site}: the refusal moved the stage charge"
        );
        assert_eq!(seeded_stage_walk(&slot), before);
        let fitted = build();
        let slot = seeded_slot(&fitted);
        let before = slot.accounted.load(Ordering::Acquire);
        assert_fitted_at(site, &fitted, &seeded_step_parked, exact);
        let after = slot.accounted.load(Ordering::Acquire);
        assert_eq!(
            slot.work.lock().unwrap().indexed,
            SEEDED_EVENTS + SEEDED_STEP
        );
        assert_eq!(
            after,
            seeded_stage_walk(&slot),
            "{site}: the stage charge is not its owned heap walked once"
        );
        assert!(
            after - before > old_reserve,
            "{site}: the copy-on-write growth {} fits the former reserve {old_reserve}",
            after - before
        );
        assert!(
            after - before <= exact,
            "{site}: the retained growth {} exceeds its admission {exact}",
            after - before
        );
        let state = fitted.store.lock_state();
        let (key, _) = state
            .classifier_stages
            .iter()
            .next()
            .expect("seeded classifier stage");
        assert_eq!(
            state.ledger.classifier,
            arc_mirror::<ClassifierSlot>() + MUTEX_STORAGE_MIRROR + key.capacity() + after,
            "{site}: the classifier gauge is not the slot, its key, and its owned heap"
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
            + arc_mirror::<crate::snapshot_ledger::ReleaseQueue>()
            + MUTEX_STORAGE_MIRROR
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
            store
                .lock_state()
                .leases
                .audit_with(NativeStore::audit_lease_bytes)
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
                NativeStore::audit_prepared_query_bytes(&token, &state.prepared_queries[&token])
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
            store
                .lock_state()
                .leases
                .audit_with(NativeStore::audit_lease_bytes)
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

fn codex_finish_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let mut fixture = Fixture {
        store: fast_store(),
        owner: context_for("codex", background),
        request: acquire(&source.path),
        pins: Vec::new(),
    };
    let (data, pinned) = codex_acquired(&fixture);
    fixture.request.insert("outcome", data);
    let identity = pinned.stamp.identity;
    fixture.pins.push(pinned);
    {
        let mut state = fixture.store.lock_state();
        let lease = state.leases.audit_with(NativeStore::audit_lease_bytes);
        let mut inode = 0;
        while state.recent_codex_growth(&identity) <= lease {
            inode += 1;
            state.insert_recent_codex(
                SourceIdentity {
                    device: 0,
                    inode,
                    window_base: 0,
                },
                now_ms(),
            );
        }
    }
    fixture
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
    let identity = SourceStamp::of(&std::fs::metadata(&source.path).unwrap()).identity;
    let site = "recent codex completion";
    for background in [false, true] {
        let build = || codex_finish_fixture(&source, background);
        let finished = |fixture: &Fixture| {
            constructed(site, &fixture.store, [1, 0], false, || {
                finish_source(fixture).map(|_| ())
            })
        };
        let cached = |fixture: &Fixture| {
            finished(fixture)
                && fixture
                    .store
                    .lock_state()
                    .recent_codex
                    .contains_key(&identity)
        };
        let probe = build();
        let predicted = facts_bound_walk(&probe.pins[0]);
        let (growth, lease) = {
            let state = probe.store.lock_state();
            assert_eq!(
                state.leases.len(),
                1,
                "{site}: the completion consumes exactly one lease"
            );
            (
                state.recent_codex.growth(1) + state.recent_codex_expiry.growth(1),
                state.leases.audit_with(NativeStore::audit_lease_bytes),
            )
        };
        assert!(
            growth > lease,
            "{site}: the consumed lease covers the cache growth: growth={growth} lease={lease}"
        );
        let exact = exact_headroom(&build, &finished);
        assert_eq!(
            exact, predicted,
            "{site}: completion is not gated on its full facts reservation"
        );
        let with_cache = exact_headroom(&build, &cached);
        assert!(
            exact < with_cache && with_cache <= exact + growth,
            "the cache write costs its growth: exact={exact} with_cache={with_cache} growth={growth}"
        );
        assert_eq!(
            with_cache,
            exact + growth - lease,
            "{site}: the cache write costs its growth beyond the consumed lease: exact={exact} growth={growth} lease={lease}"
        );
        {
            let refused = build();
            let leases = lease_table(&refused.store).0;
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!finished(&refused));
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap_for(&refused.owner));
            let state = refused.store.lock_state();
            assert_eq!(
                state.leases.len(),
                leases - 1,
                "{site}: the refusal leaked its source lease"
            );
            assert!(!state.prepared_facts.contains_key(&identity));
            assert_eq!(
                state.latest.contains_key(&identity),
                state.recent_codex.contains_key(&identity),
                "{site}: the refusal split the raw codex cache from its snapshot"
            );
        }
        {
            let skipped = build();
            let _filler = fill_to(&skipped.store, &skipped.owner, with_cache - 1);
            let reserved = {
                let state = skipped.store.lock_state();
                (
                    state.recent_codex.reserved(),
                    state.recent_codex_expiry.reserved(),
                )
            };
            assert!(
                finished(&skipped),
                "{site}: the completion itself was refused"
            );
            skipped.store.assert_conserved();
            assert!(audited(&skipped.store)[TOTAL] <= cap_for(&skipped.owner));
            let state = skipped.store.lock_state();
            assert!(!state.latest.contains_key(&identity));
            assert_eq!(
                (
                    state.recent_codex.reserved(),
                    state.recent_codex_expiry.reserved(),
                ),
                reserved,
                "{site}: the skipped cache write grew its tables"
            );
            assert_eq!(state.recent_codex_raw_bytes, 0);
        }
        let admitted = build();
        let _filler = fill_to(&admitted.store, &admitted.owner, with_cache);
        assert!(cached(&admitted));
        admitted.store.assert_conserved();
        assert!(audited(&admitted.store)[TOTAL] <= cap_for(&admitted.owner));
        let state = admitted.store.lock_state();
        assert!(state.latest.contains_key(&identity));
        assert_eq!(
            state.recent_codex_raw_bytes,
            admitted.pins[0].codex_raw.as_ref().unwrap().len()
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

fn resolve_request(scenario: &Scenario, first: usize, padding: usize) -> Value {
    let ids = scenario.ids();
    let session_ids = vec![ids[first].clone(), ids[1 - first].clone()];
    json!({"schema":SCHEMA,"id":"ledger-resolve","operation":"resolve","session_ids":session_ids,"roots":scenario.roots(),"classifier":{"id":"native","version":"1"},"padding":"p".repeat(padding),"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()})
}

fn resolution_fixture(
    scenario: &Scenario,
    background: bool,
    cached: bool,
    padding: usize,
) -> Fixture {
    let store = slow_store();
    let owner = context_for("resolution", background);
    let (handle, snapshot) = acquired(&store, &scenario.sidechains[0], &owner);
    release_lease(&store, &handle, &owner);
    Fixture {
        store,
        owner,
        request: resolve_request(scenario, usize::from(!cached), padding),
        pins: vec![snapshot],
    }
}

fn assert_owner_released(store: &NativeStore, site: &str) {
    let state = store.lock_state();
    assert!(
        state.waiters.is_empty(),
        "{site}: a pending source waiter outlived its failed owner"
    );
    assert!(
        state.loads.is_empty(),
        "{site}: a pending source load outlived its failed owner"
    );
    assert!(
        state.leases.is_empty(),
        "{site}: a lease issued to the failed owner outlived it"
    );
}

fn refused_resolution(site: &str, fixture: &Fixture, opens: u64, cache_hits: u64) -> bool {
    let response = submit(fixture);
    if parked(&response) {
        return true;
    }
    let usage = &response["usage"];
    assert_eq!(
        usage["requests_failed"].as_u64(),
        Some(1),
        "{site}: {response:?}"
    );
    assert_eq!(
        usage["source_opens"].as_u64(),
        Some(opens),
        "{site}: {response:?}"
    );
    assert_eq!(
        usage["cache_hits"].as_u64(),
        Some(cache_hits),
        "{site}: {response:?}"
    );
    assert_eq!(
        usage["source_bytes_read"].as_u64().unwrap() > 0,
        opens > cache_hits,
        "{site}: {response:?}"
    );
    assert_owner_released(&fixture.store, site);
    false
}

fn attached_graph_fixture(source: &LedgerSource, member: &Path, background: bool) -> Fixture {
    let store = cursor_store();
    let owner = context_for("graph", background);
    let (root, root_snapshot) = acquired(&store, &source.path, &owner);
    Fixture {
        request: json!({"schema":SCHEMA,"id":"ledger-graph","operation":"query","view":{"handle":root,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[member.to_string_lossy().as_ref()]},"query":missing_tool(),"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        store,
        owner,
        pins: vec![root_snapshot],
    }
}

fn parked_graph_member(fixture: &Fixture) -> (Value, String) {
    let mut page = submit(fixture);
    loop {
        let cursor = page["cursor"]
            .as_str()
            .unwrap_or_else(|| panic!("the graph settled before parking its member: {page:?}"))
            .to_owned();
        let held = fixture
            .store
            .lock_state()
            .graphs
            .get(&cursor)
            .and_then(|graph| graph.pending.as_ref().map(|pending| pending.token.clone()));
        if let Some(token) = held {
            assert!(fixture.store.lock_state().waiters.contains_key(&token));
            return (page, token);
        }
        page = resume(&fixture.store, &page, &fixture.owner);
    }
}

fn fail_next_pin(store: &NativeStore, spared_lease: Option<String>) {
    *store.pin_hook.lock().unwrap() = Some(Arc::new(move |handle: &Value| {
        if handle["lease_id"].as_str() == spared_lease.as_deref() {
            return Ok(());
        }
        Err(SnapshotError::new(Status::Deadline, "pin hook"))
    }));
}

fn assert_pin_failed(site: &str, response: &Value) {
    assert_eq!(
        response["status"].as_str(),
        Some("deadline"),
        "{site}: {response:?}"
    );
    assert_eq!(
        response["reason"].as_str(),
        Some("pin hook"),
        "{site}: {response:?}"
    );
}

#[test]
fn refused_resolution_park_releases_its_pending_source_and_lease() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    let padding = 64 * 1024;
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for cached in [false, true] {
        for background in [false, true] {
            let site = format!("resolution park cached={cached} background={background}");
            let build = |padding: usize| resolution_fixture(&scenario, background, cached, padding);
            let small = exact_headroom(&|| build(0), &attempt);
            let large = exact_headroom(&|| build(padding), &attempt);
            assert_eq!(
                large - small,
                padding,
                "{site}: the parked cursor is not the binding admission"
            );
            assert_refused_at(
                &site,
                &build(padding),
                &|fixture: &Fixture| refused_resolution(&site, fixture, 0, 0),
                1,
            );
            assert_refused_at(
                &site,
                &build(padding),
                &|fixture: &Fixture| refused_resolution(&site, fixture, 1, u64::from(cached)),
                large,
            );
            let fitted = build(padding);
            let _filler = fill_to(&fitted.store, &fitted.owner, large);
            let response = submit(&fitted);
            assert!(
                parked(&response),
                "{site}: the exact fit was refused: {response:?}"
            );
            fitted.store.assert_conserved();
            assert!(
                audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner),
                "{site}"
            );
            let cursor = response["cursor"].as_str().unwrap();
            {
                let state = fitted.store.lock_state();
                let pending = state
                    .resolutions
                    .get(cursor)
                    .expect("the parked resolution")
                    .pending
                    .clone();
                assert_eq!(pending.is_none(), cached, "{site}");
                assert_eq!(state.waiters.len(), usize::from(!cached), "{site}");
                assert!(
                    pending.is_none_or(|token| state.waiters.contains_key(&token)),
                    "{site}"
                );
                assert_eq!(state.leases.len(), usize::from(cached), "{site}");
            }
            release_cursor(&fitted.store, cursor, &fitted.owner);
            assert!(fitted.store.lock_state().waiters.is_empty(), "{site}");
            fitted.store.assert_conserved();
        }
    }
}

#[test]
fn refused_resolution_resume_releases_its_pending_source() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let site = format!("resolution resume background={background}");
        let build = || {
            let fixture = resolution_fixture(&scenario, background, false, 0);
            let first = submit(&fixture);
            assert!(parked(&first), "{site}: {first:?}");
            let cursor = first["cursor"].as_str().unwrap().to_owned();
            let pending = {
                let state = fixture.store.lock_state();
                let pending = state
                    .resolutions
                    .get(&cursor)
                    .expect("the parked resolution")
                    .pending
                    .clone()
                    .expect("the parked resolution carries its pending source");
                assert!(state.waiters.contains_key(&pending), "{site}");
                assert_eq!(state.loads.len(), 1, "{site}");
                pending
            };
            fixture.store.assert_conserved();
            (fixture, cursor, pending)
        };
        let (refused, cursor, pending) = build();
        let outcome = {
            let _filler = fill_to(&refused.store, &refused.owner, 0);
            refused.store.dispatch(
                &resume_request(&cursor),
                &refused.owner,
                &Cancellation::default(),
                &mut [0u64; 18],
            )
        };
        assert!(
            matches!(&outcome, Err(error) if error.status == Status::RetainedLimit),
            "{site}: {outcome:?}"
        );
        refused.store.assert_conserved();
        {
            let state = refused.store.lock_state();
            assert!(
                state.resolutions.is_empty(),
                "{site}: a refused resume left its cursor parked"
            );
            assert!(
                !state.waiters.contains_key(&pending),
                "{site}: the refused resume left its pending source waiter behind"
            );
            assert!(state.waiters.is_empty(), "{site}");
            assert!(
                state.loads.is_empty(),
                "{site}: the pending source load outlived its resolution"
            );
        }
        let (control, cursor, pending) = build();
        {
            let mut state = control.store.lock_state();
            state.resolutions.remove(&cursor);
            state.waiters.remove(&pending);
            NativeStore::prune(&mut state);
        }
        evict_unpinned(&control.store, &control.owner);
        assert_eq!(
            (
                settled(&refused.store),
                audited(&refused.store),
                bookkeeping(&refused.store)
            ),
            (
                settled(&control.store),
                audited(&control.store),
                bookkeeping(&control.store)
            ),
            "{site}: a refused resume did more than consume its parked cursor and its pending source"
        );
        assert!(audited(&refused.store)[TOTAL] <= cap_for(&refused.owner));
    }
}

#[test]
fn lease_capped_resolution_park_releases_its_pending_source_and_lease() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    for cached in [false, true] {
        for background in [false, true] {
            let site =
                format!("lease-capped resolution park cached={cached} background={background}");
            let cap = 16 - usize::from(background);
            for fillers in [cap - 1, cap] {
                let fixture = resolution_fixture(&scenario, background, cached, 0);
                for _ in 0..fillers {
                    let filler = fixture.store.request(
                        &resolve_request(&scenario, 0, 0),
                        &fixture.owner,
                        &Cancellation::default(),
                    );
                    assert!(parked(&filler), "{site}: {filler:?}");
                    release_lease(
                        &fixture.store,
                        &filler["data"]["sessions"][0]["description"]["handle"],
                        &fixture.owner,
                    );
                }
                assert_eq!(
                    fixture.store.lock_state().resolutions.len(),
                    fillers,
                    "{site}"
                );
                let before = (
                    settled(&fixture.store),
                    audited(&fixture.store),
                    bookkeeping(&fixture.store),
                );
                let response = submit(&fixture);
                if fillers < cap {
                    assert!(
                        parked(&response),
                        "{site}: the last cursor slot was refused: {response:?}"
                    );
                    continue;
                }
                assert_eq!(
                    response["status"].as_str(),
                    Some("lease_limit"),
                    "{site}: {response:?}"
                );
                assert_eq!(
                    response["reason"].as_str(),
                    Some("resolution cursor admission exhausted"),
                    "{site}: {response:?}"
                );
                assert_eq!(
                    response["usage"]["source_opens"].as_u64(),
                    Some(1),
                    "{site}: {response:?}"
                );
                assert_eq!(
                    response["usage"]["cache_hits"].as_u64(),
                    Some(u64::from(cached)),
                    "{site}: {response:?}"
                );
                assert_owner_released(&fixture.store, &site);
                fixture.store.assert_conserved();
                assert_eq!(
                    (
                        ledger(&fixture.store),
                        audited(&fixture.store),
                        bookkeeping(&fixture.store)
                    ),
                    before,
                    "{site}: the lease-capped park leaked state"
                );
            }
        }
    }
}

#[test]
fn failed_resolution_pages_release_their_pending_source_and_lease() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    for background in [false, true] {
        for cached in [false, true] {
            let site = format!("output-limited resolution cached={cached} background={background}");
            let fixture = resolution_fixture(&scenario, background, cached, 0);
            let mut request = fixture.request.clone();
            request["limits"].insert("max_output_bytes", json!(64));
            let before = (
                settled(&fixture.store),
                audited(&fixture.store),
                bookkeeping(&fixture.store),
            );
            let response =
                fixture
                    .store
                    .request(&request, &fixture.owner, &Cancellation::default());
            assert_eq!(
                response["status"].as_str(),
                Some("output_limit"),
                "{site}: {response:?}"
            );
            assert_eq!(
                response["usage"]["source_opens"].as_u64(),
                Some(1),
                "{site}: {response:?}"
            );
            assert_owner_released(&fixture.store, &site);
            fixture.store.assert_conserved();
            assert_eq!(
                (
                    ledger(&fixture.store),
                    audited(&fixture.store),
                    bookkeeping(&fixture.store)
                ),
                before,
                "{site}: the output-limited page leaked state"
            );
        }
        let site = format!("stale resumed resolution background={background}");
        let fixture = resolution_fixture(&scenario, background, true, 0);
        let first = submit(&fixture);
        assert!(parked(&first), "{site}: {first:?}");
        let second = resume(&fixture.store, &first, &fixture.owner);
        assert!(parked(&second), "{site}: {second:?}");
        let cursor = second["cursor"].as_str().unwrap();
        let pending = fixture
            .store
            .lock_state()
            .resolutions
            .get(cursor)
            .and_then(|resolution| resolution.pending.clone())
            .expect("the second page parks a pending source");
        assert!(
            fixture.store.lock_state().waiters.contains_key(&pending),
            "{site}"
        );
        release_lease(
            &fixture.store,
            &first["data"]["sessions"][0]["description"]["handle"],
            &fixture.owner,
        );
        let failed = resume(&fixture.store, &second, &fixture.owner);
        assert_eq!(
            failed["status"].as_str(),
            Some("stale_handle"),
            "{site}: {failed:?}"
        );
        assert_owner_released(&fixture.store, &site);
        fixture.store.assert_conserved();
    }
}

#[test]
fn failed_resolution_pin_releases_its_candidate_lease() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let site = format!("resolution pin background={background}");
        let fixture = resolution_fixture(&scenario, background, true, 0);
        let before = (
            settled(&fixture.store),
            audited(&fixture.store),
            bookkeeping(&fixture.store),
        );
        fail_next_pin(&fixture.store, None);
        let response = submit(&fixture);
        assert_pin_failed(&site, &response);
        assert!(
            fixture.store.pin_hook.lock().unwrap().is_none(),
            "{site}: the candidate pin never ran"
        );
        assert_eq!(
            response["usage"]["source_opens"].as_u64(),
            Some(1),
            "{site}: {response:?}"
        );
        assert_eq!(
            response["usage"]["cache_hits"].as_u64(),
            Some(1),
            "{site}: {response:?}"
        );
        assert_owner_released(&fixture.store, &site);
        assert!(fixture.store.lock_state().resolutions.is_empty(), "{site}");
        fixture.store.assert_conserved();
        assert_eq!(
            (
                ledger(&fixture.store),
                audited(&fixture.store),
                bookkeeping(&fixture.store)
            ),
            before,
            "{site}: the failed candidate pin leaked state"
        );
    }
}

#[test]
fn changed_registered_member_releases_its_pending_source_waiter() {
    let scenario = Scenario::new(2, |index| line(&format!("thread-{index:04}")));
    let store = slow_store();
    let owner = context_for("warming", true);
    let changed = scenario.sidechains[1].clone();
    let recorded = SourceStamp::of(&std::fs::metadata(&changed).unwrap());
    let appended = lines(0..2);
    *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&changed)
            .unwrap()
            .write_all(appended.as_bytes())
            .unwrap();
    }));
    let response = store.request(&warm_request(&scenario), &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("changed"), "{response:?}");
    assert_eq!(
        response["reason"].as_str(),
        Some("registered source changed"),
        "{response:?}"
    );
    assert!(
        store.read_hook.lock().unwrap().is_none(),
        "the barrier never fired between membership and open"
    );
    let current = SourceStamp::of(&std::fs::metadata(&scenario.sidechains[1]).unwrap());
    assert_ne!(current, recorded);
    {
        let state = store.lock_state();
        assert!(
            state.waiters.is_empty(),
            "the changed member left its pending source waiter behind"
        );
        assert!(
            state.leases.is_empty(),
            "the warm left an internal source lease behind"
        );
        assert_eq!(
            state.loads.len(),
            1,
            "a load outlived the failed warm outside the prepared-load cache"
        );
        let slot = state
            .loads
            .get(&current.identity)
            .expect("the changed revision's load");
        assert_eq!(slot.stamp, current);
        let (pinned, _) = state
            .prepared_loads
            .get(&current.identity)
            .expect("the prepared-load cache pin");
        assert!(Arc::ptr_eq(slot, pinned));
        assert_eq!(
            Arc::strong_count(slot),
            2,
            "a waiter still holds the changed revision's load"
        );
    }
    store.assert_conserved();
    assert!(audited(&store)[TOTAL] <= cap_for(&owner));
    warm(&store, &scenario, &owner);
    assert!(store.lock_state().waiters.is_empty());
    assert_eq!(
        settled(&store)[LOADS],
        0,
        "the rebuilt membership never consumed the pinned load"
    );
    store.assert_conserved();
}

#[test]
fn vanished_graph_member_releases_its_pending_source_waiter() {
    for background in [false, true] {
        let site = format!("vanished graph member background={background}");
        let source = LedgerSource::new(&line("root"));
        let member = source.file("agent-a.jsonl", &lines(0..4));
        let fixture = attached_graph_fixture(&source, &member, background);
        let (page, pending) = parked_graph_member(&fixture);
        std::fs::remove_file(&member).unwrap();
        let failed = resume(&fixture.store, &page, &fixture.owner);
        assert_eq!(
            failed["status"].as_str(),
            Some("missing"),
            "{site}: {failed:?}"
        );
        {
            let state = fixture.store.lock_state();
            assert!(
                !state.waiters.contains_key(&pending),
                "{site}: the vanished member left its pending source waiter behind"
            );
            assert!(state.waiters.is_empty(), "{site}");
            assert_eq!(
                state.leases.len(),
                1,
                "{site}: only the client's root lease survives the failed page"
            );
            assert!(
                state.leases.contains_key(
                    fixture.request["view"]["handle"]["lease_id"]
                        .as_str()
                        .unwrap()
                ),
                "{site}"
            );
            assert!(
                state.loads.is_empty(),
                "{site}: the vanished member's load outlived the failed page"
            );
            assert!(state.graphs.is_empty(), "{site}");
        }
        fixture.store.assert_conserved();
        assert_eq!(settled(&fixture.store)[LOADS], 0, "{site}");
    }
}

#[test]
fn failed_graph_member_pin_releases_its_internal_lease() {
    for background in [false, true] {
        let site = format!("graph member pin background={background}");
        let source = LedgerSource::new(&line("root"));
        let member = source.file("agent-a.jsonl", &lines(0..4));
        let fixture = attached_graph_fixture(&source, &member, background);
        let (page, pending) = parked_graph_member(&fixture);
        let root_lease = fixture.request["view"]["handle"]["lease_id"]
            .as_str()
            .unwrap()
            .to_owned();
        fail_next_pin(&fixture.store, Some(root_lease.clone()));
        let failed = settle(&fixture.store, page, &fixture.owner);
        assert_pin_failed(&site, &failed);
        assert!(
            fixture.store.pin_hook.lock().unwrap().is_none(),
            "{site}: the member pin never ran"
        );
        {
            let state = fixture.store.lock_state();
            assert!(
                !state.waiters.contains_key(&pending),
                "{site}: the member's pending source waiter outlived the failed pin"
            );
            assert!(state.waiters.is_empty(), "{site}");
            assert_eq!(
                state.leases.len(),
                1,
                "{site}: only the client's root lease survives the failed page"
            );
            assert!(state.leases.contains_key(&root_lease), "{site}");
            assert!(state.loads.is_empty(), "{site}");
            assert!(state.graphs.is_empty(), "{site}");
        }
        fixture.store.assert_conserved();
    }
}

#[test]
fn failed_prepared_source_pin_releases_its_internal_lease() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let site = format!("finish_prepared_source pin background={background}");
        let fixture = finish_fixture(prepared_store(), &scenario.sidechains[0], background);
        assert_eq!(settled(&fixture.store)[LEASES], 1, "{site}");
        fail_next_pin(&fixture.store, None);
        let Err(error) = finish_source(&fixture) else {
            panic!("{site}: the forced pin failure was adopted");
        };
        assert_eq!(
            (error.status, error.reason.as_str()),
            (Status::Deadline, "pin hook"),
            "{site}"
        );
        assert!(fixture.store.pin_hook.lock().unwrap().is_none(), "{site}");
        {
            let state = fixture.store.lock_state();
            assert!(
                state.leases.is_empty(),
                "{site}: the internal source lease outlived the failed pin"
            );
            assert!(state.waiters.is_empty(), "{site}");
            assert!(state.prepared_loads.is_empty(), "{site}");
        }
        assert_eq!(
            fact_probes(&fixture.store),
            [0, 0],
            "{site}: the failed pin reached fact construction"
        );
        fixture.store.assert_conserved();
        assert_eq!(ledger(&fixture.store)[LEASES], 0, "{site}");
    }
}

#[test]
fn cancelled_registered_warming_releases_its_location_cursor() {
    let scenario = Scenario::new(4, |index| line(&format!("thread-{index:04}")));
    let store = cursor_store();
    let owner = context_for("warming", true);
    let cancel = Cancellation::default();
    let armed = cancel.clone();
    *store.locate_hook.lock().unwrap() = Some(Arc::new(move || armed.cancel()));
    let response = store.request(&warm_request(&scenario), &owner, &cancel);
    assert_eq!(
        response["status"].as_str(),
        Some("cancelled"),
        "{response:?}"
    );
    assert!(
        store.locate_hook.lock().unwrap().is_none(),
        "the location page never parked"
    );
    assert_eq!(
        response["usage"]["discovery_entries_examined"].as_u64(),
        Some(2),
        "{response:?}"
    );
    assert!(
        store.lock_state().locates.is_empty(),
        "the cancelled warm left its location cursor behind"
    );
    store.assert_conserved();
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
    let request = graph_query(&graph, missing_tool(), json!([]));
    let primed = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(primed["status"].as_str(), Some("ok"), "{primed:?}");
    let identity = SourceStamp::of(&std::fs::metadata(&scenario.sidechains[0]).unwrap()).identity;
    store
        .lock_state()
        .remove_prepared_facts(&identity)
        .expect("primed sidechain facts");
    Fixture {
        store,
        owner,
        request,
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

fn fact_probes(store: &NativeStore) -> [usize; 2] {
    [
        store.fact_builds.load(Ordering::Relaxed),
        store.fact_lookups.load(Ordering::Relaxed),
    ]
}

fn chunk_entry_bytes(snapshot: &Arc<TranscriptSnapshot>) -> usize {
    snapshot
        .chunks
        .iter()
        .map(|chunk| {
            arc_mirror::<EntryChunk>()
                + arc_mirror::<ChunkRows>()
                + chunk.entries.capacity() * size_of::<Entry>()
                + chunk.entry_charges.capacity() * size_of::<MemoryCharge>()
                + chunk
                    .entry_charges
                    .iter()
                    .map(|charge| charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
                    .sum::<usize>()
        })
        .sum()
}

pub(super) fn facts_bound_walk(snapshot: &Arc<TranscriptSnapshot>) -> usize {
    let entries: Vec<&Entry> = (0..snapshot.event_count)
        .map(|index| snapshot.entry(index))
        .collect();
    let calls: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| assistant.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse(tool) => Some(parse_tool_call(&tool.name, &tool.input)),
            _ => None,
        })
        .map(|call| {
            let edits = lower_edit(&call);
            (call, edits)
        })
        .collect();
    let inputs = json!({
        "calls":calls.iter().map(|(call, _)| json!([call.name(),call.file_paths()])).collect::<Vec<_>>(),
        "commands":calls.iter().filter_map(|(call, _)| match call {
            ToolCall::Bash(bash) => Some(bash.command.as_str()),
            _ => None,
        }).collect::<Vec<_>>(),
        "edited_files":calls.iter().flat_map(|(_, edits)| edits.iter().map(|(path, _)| json!({"path":path}))).collect::<Vec<_>>(),
        "skills":calls.iter().filter_map(|(call, _)| match call {
            ToolCall::Skill(skill) => Some(skill.skill.as_str()),
            _ => None,
        }).collect::<Vec<_>>()
    });
    let mut events = Vec::new();
    for entry in &entries {
        let (text, tools) = match entry {
            Entry::User(user) => {
                let mut text = user.content.text();
                text.reserve_exact(
                    user.tool_results()
                        .map(|result| result.content.len())
                        .sum::<usize>(),
                );
                for result in user.tool_results() {
                    text.push_str(&result.content);
                }
                (text, Vec::new())
            }
            Entry::Assistant(assistant) => (
                joined_text(&assistant.blocks),
                assistant
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse(tool) => Some(tool.name.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            Entry::System(system) => (system.content.clone().unwrap_or_default(), Vec::new()),
            _ => (String::new(), Vec::new()),
        };
        events.push(OverrideEvent { text, tools });
    }
    NativeStore::audit_facts_bytes(&PreparedFacts {
        inputs,
        has_error: false,
        override_events: Some(events),
        accounted: 0,
    })
}

fn built_facts(source: &Arc<TranscriptSnapshot>, selectors: &Value) -> PreparedFacts {
    crate::snapshot_projection::prepare_facts(
        source,
        selectors,
        &work_bounds(),
        &Cancellation::default(),
    )
    .unwrap()
    .0
}

fn disk_key(owner: &Value, stamp: SourceStamp) -> PreparedDiskKey {
    PreparedDiskKey::new(
        stamp,
        owner["registry_generation"].as_str().unwrap(),
        "hook",
        &owner["authority"],
        &json!({"id":"native","version":"1"}),
    )
    .unwrap()
}

fn decode_transients_walk(file: usize) -> usize {
    let payload = file - crate::snapshot_prepared_disk::HEADER_BYTES;
    let node_buffer = size_of::<Vec<Value>>() + (payload / 2 + 2) * size_of::<Value>();
    let unescape_scratch = 2 * (payload + 32);
    node_buffer + unescape_scratch
}

fn decoded_facts_prediction(fixture: &Fixture, source: &Arc<TranscriptSnapshot>) -> usize {
    let built = built_facts(source, &json!([]));
    let decoded: PreparedFacts = sonic_rs::from_slice(&sonic_rs::to_vec(&built).unwrap()).unwrap();
    let (decoded_walk, built_walk) = (
        NativeStore::audit_facts_bytes(&decoded),
        NativeStore::audit_facts_bytes(&built),
    );
    assert!(
        decoded_walk <= built_walk,
        "the decoded facts walk {decoded_walk} outgrew the {built_walk}-byte built facts"
    );
    let file = fixture
        .store
        .prepared_disk
        .entry_file_len(&disk_key(&fixture.owner, source.stamp)) as usize;
    file + built_walk + decode_transients_walk(file)
}

fn decoded_root_facts_prediction(fixture: &Fixture) -> usize {
    facts_bound_walk(&fixture.pins[0]).max(decoded_facts_prediction(fixture, &fixture.pins[0]))
}

fn decoded_source_facts_prediction(fixture: &Fixture) -> usize {
    decoded_facts_prediction(fixture, &fixture.pins[1])
}

fn constructed(
    site: &str,
    store: &NativeStore,
    built: [usize; 2],
    refusable_after: bool,
    attempt: impl FnOnce() -> Result<(), SnapshotError>,
) -> bool {
    let before = fact_probes(store);
    let result = attempt();
    let after = fact_probes(store);
    let moved = [
        after[BUILDS] - before[BUILDS],
        after[LOOKUPS] - before[LOOKUPS],
    ];
    match result {
        Err(error) if error.status != Status::RetainedLimit => {
            panic!("{site}: failed outside admission: {error:?}")
        }
        Err(_) if moved == [0, 0] => false,
        Err(error) if !refusable_after => {
            panic!("{site}: refused after constructing its facts: {error:?}")
        }
        _ => {
            assert_eq!(
                moved, built,
                "{site}: the admitted attempt did not construct exactly once"
            );
            true
        }
    }
}

fn assert_refusal_contract(site: &str, response: &Value) {
    assert_eq!(
        response["status"].as_str(),
        Some("retained_limit"),
        "{site}: {response:?}"
    );
    assert_eq!(
        response["reason"].as_str(),
        Some("accounted storage admission exhausted"),
        "{site}: {response:?}"
    );
    assert_eq!(
        response["complete"].as_bool(),
        Some(false),
        "{site}: {response:?}"
    );
    assert!(
        response["data"].is_null() && response["cursor"].is_null(),
        "{site}: {response:?}"
    );
    for (counter, expected) in [
        ("requests_failed", 1),
        ("requests_cancelled", 0),
        ("source_bytes_read", 0),
        ("events_parsed", 0),
        ("cold_parses", 0),
        ("generations_published", 0),
    ] {
        assert_eq!(
            response["usage"][counter].as_u64(),
            Some(expected),
            "{site}: {counter}: {response:?}"
        );
    }
}

fn submitted(site: &str, fixture: &Fixture, built: [usize; 2]) -> bool {
    let before = fact_probes(&fixture.store);
    let response = submit(fixture);
    let after = fact_probes(&fixture.store);
    let moved = [
        after[BUILDS] - before[BUILDS],
        after[LOOKUPS] - before[LOOKUPS],
    ];
    if moved == [0, 0] {
        assert_refusal_contract(site, &response);
        return false;
    }
    assert_eq!(moved, built, "{site}: {response:?}");
    assert!(
        matches!(
            response["status"].as_str(),
            Some("ok" | "incomplete" | "retained_limit")
        ),
        "{site}: {response:?}"
    );
    true
}

fn assert_refused_reply(site: &str, fixture: &Fixture, headroom: usize, cache_hits: u64) {
    let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
    let failed = fixture.store.lock_state().counters[12];
    let response = submit(fixture);
    assert_refusal_contract(site, &response);
    assert_eq!(
        response["usage"]["cache_hits"].as_u64(),
        Some(cache_hits),
        "{site}: {response:?}"
    );
    assert_eq!(fixture.store.lock_state().counters[12], failed + 1);
    fixture.store.assert_conserved();
}

fn load_slot_ids(store: &NativeStore) -> HashSet<String> {
    store
        .lock_state()
        .loads
        .values()
        .map(|slot| slot.id.clone())
        .collect()
}

fn cached_slot_walk(slot: &LoadSlot) -> usize {
    assert!(
        slot.work.lock().unwrap().result.is_some(),
        "the residue load slot does not hold a cached snapshot"
    );
    NativeStore::audit_load_record_bytes(slot) + empty_index_walk(slot)
}

fn residue_walk(store: &NativeStore, held: &HashSet<String>) -> (usize, usize) {
    let residue: Vec<_> = store
        .lock_state()
        .loads
        .values()
        .filter(|slot| !held.contains(&slot.id))
        .cloned()
        .collect();
    (
        residue.len(),
        residue.iter().map(|slot| cached_slot_walk(slot)).sum(),
    )
}

fn assert_facts_admitted_in_full(
    site: &str,
    build: &dyn Fn() -> Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    offset: usize,
    predicted: usize,
    reserved: &[Trace],
    residue: LoadResidue,
) {
    assert!(predicted > 0, "{site}: nothing was predicted");
    assert_eq!(
        exact_headroom(build, attempt),
        offset + predicted,
        "{site}: construction is not gated on the full {predicted}-byte prediction"
    );
    for headroom in [0, predicted - 1] {
        let fixture = build();
        let cap = cap_for(&fixture.owner);
        let filler = fill_to(&fixture.store, &fixture.owner, offset + headroom);
        fixture.store.assert_conserved();
        let before = ledger(&fixture.store);
        let before_audited = audited(&fixture.store);
        let before_bookkeeping = bookkeeping(&fixture.store);
        let probes = fact_probes(&fixture.store);
        let disk = fixture.store.prepared_disk.stats();
        let leases = lease_table(&fixture.store);
        let slot_ids = load_slot_ids(&fixture.store);
        let held = {
            let state = fixture.store.lock_state();
            (
                state.prepared_facts.len(),
                state.waiters.len(),
                state.prepared_loads.len(),
            )
        };
        traced(&fixture.store);
        assert!(
            !attempt(&fixture),
            "{site}: headroom {headroom} below the prediction was admitted"
        );
        let trace = traced(&fixture.store);
        assert_eq!(
            trace
                .iter()
                .filter(|entry| matches!(entry, Trace::Reserved(_)))
                .copied()
                .collect::<Vec<_>>(),
            reserved,
            "{site}: the refusal at headroom {headroom} reserved facts bytes: {trace:?}"
        );
        assert!(
            !trace
                .iter()
                .any(|entry| matches!(entry, Trace::Allocated(_))),
            "{site}: the refusal at headroom {headroom} allocated bookkeeping: {trace:?}"
        );
        assert!(offset > 0 || trace.is_empty(), "{site}: {trace:?}");
        fixture.store.assert_conserved();
        let after = ledger(&fixture.store);
        let (slots, slot_bytes) = residue_walk(&fixture.store, &slot_ids);
        assert!(
            slots <= residue.slots,
            "{site}: the refusal at headroom {headroom} left {slots} load slots behind"
        );
        let [expected, expected_audited] = [before, before_audited].map(|mut gauges| {
            gauges[LOADS] += slots;
            gauges[PENDING] += slot_bytes;
            gauges[TOTAL] += slot_bytes;
            gauges
        });
        assert_eq!(
            (after, audited(&fixture.store), bookkeeping(&fixture.store)),
            (expected, expected_audited, before_bookkeeping),
            "{site}: the refusal at headroom {headroom} leaked state"
        );
        assert!(
            audited(&fixture.store)[TOTAL] <= cap,
            "{site}: the refusal left the audit above the cap"
        );
        assert_eq!(
            fact_probes(&fixture.store),
            probes,
            "{site}: facts were built or decoded before the refusal at headroom {headroom}"
        );
        let stats = fixture.store.prepared_disk.stats();
        assert_eq!(
            (stats.entries, stats.bytes, stats.writes),
            (disk.entries, disk.bytes, disk.writes),
            "{site}: the refusal wrote the facts disk cache"
        );
        assert_eq!(
            lease_table(&fixture.store),
            leases,
            "{site}: the refusal leaked a lease"
        );
        drop(filler);
        let pinned = {
            let state = fixture.store.lock_state();
            assert_eq!(
                (state.prepared_facts.len(), state.waiters.len()),
                (held.0, held.1),
                "{site}: the refusal left facts or a waiter behind"
            );
            assert_eq!(
                state.transient_bytes, 0,
                "{site}: the refusal left a reservation behind"
            );
            state.prepared_loads.len() - held.2
        };
        assert!(
            pinned <= residue.pinned,
            "{site}: the refusal at headroom {headroom} pinned {pinned} prepared loads"
        );
        assert_eq!(
            settled(&fixture.store)[LOADS],
            before[LOADS] + pinned,
            "{site}: the refusal left a load slot that is neither pinned nor pruned"
        );
    }
    assert_fitted_at(site, &build(), attempt, offset + predicted);
}

fn root_facts_fixture(source: &LedgerSource, background: bool, primed: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("root-facts", background);
    let (root, snapshot) = acquired(&store, &source.path, &owner);
    let classifier = json!({"id":"native","version":"1"});
    if primed {
        store
            .prepared_root_facts(
                &snapshot,
                &classifier,
                &owner,
                &work_bounds(),
                &Cancellation::default(),
            )
            .unwrap();
        store
            .lock_state()
            .remove_prepared_facts(&snapshot.stamp.identity)
            .expect("primed root facts");
    }
    assert_eq!(
        store
            .prepared_disk
            .has_entry(&disk_key(&owner, snapshot.stamp))
            .unwrap(),
        primed
    );
    assert!(!store
        .lock_state()
        .prepared_facts
        .contains_key(&snapshot.stamp.identity));
    Fixture {
        store,
        owner,
        request: prepare_request(&root, &[], &[], &[]),
        pins: vec![snapshot],
    }
}

fn root_facts_prediction(fixture: &Fixture, primed: bool) -> usize {
    if primed {
        decoded_root_facts_prediction(fixture)
    } else {
        facts_bound_walk(&fixture.pins[0])
    }
}

#[test]
fn root_facts_are_admitted_in_full_before_construction_or_decoding() {
    let source = LedgerSource::new(&lines(0..4));
    for background in [false, true] {
        for (primed, built) in [(false, [1, 0]), (true, [0, 1])] {
            let build = || root_facts_fixture(&source, background, primed);
            let probe = build();
            assert_eq!(
                settled(&probe.store)[ENTRIES],
                entry_bytes(&[&probe.pins[0]])
            );
            assert_facts_admitted_in_full(
                "prepared_root_facts",
                &build,
                &|fixture: &Fixture| {
                    constructed("prepared_root_facts", &fixture.store, built, false, || {
                        fixture
                            .store
                            .prepared_root_facts(
                                &fixture.pins[0],
                                &json!({"id":"native","version":"1"}),
                                &fixture.owner,
                                &work_bounds(),
                                &Cancellation::default(),
                            )
                            .map(|_| ())
                    })
                },
                0,
                root_facts_prediction(&probe, primed),
                &[],
                NO_LOADS,
            );
        }
    }
}

#[test]
fn empty_root_facts_are_admitted_at_their_fixed_overhead_before_construction() {
    let source = LedgerSource::new("");
    let site = "prepared_root_facts empty root";
    for background in [false, true] {
        let build = || root_facts_fixture(&source, background, false);
        let probe = build();
        assert_eq!(probe.pins[0].event_count, 0);
        assert_eq!(chunk_entry_bytes(&probe.pins[0]), 0);
        let predicted = facts_bound_walk(&probe.pins[0]);
        assert_eq!(
            predicted,
            NativeStore::audit_facts_bytes(&built_facts(&probe.pins[0], &json!([]))),
            "{site}: the fixed overhead is not the empty facts"
        );
        assert_facts_admitted_in_full(
            site,
            &build,
            &|fixture: &Fixture| {
                constructed(site, &fixture.store, [1, 0], false, || {
                    fixture
                        .store
                        .prepared_root_facts(
                            &fixture.pins[0],
                            &json!({"id":"native","version":"1"}),
                            &fixture.owner,
                            &work_bounds(),
                            &Cancellation::default(),
                        )
                        .map(|_| ())
                })
            },
            0,
            predicted,
            &[],
            NO_LOADS,
        );
    }
}

#[test]
fn prepare_graph_refuses_root_facts_before_construction_or_decoding() {
    let source = LedgerSource::new(&lines(0..4));
    let site = "prepare_graph root facts";
    for background in [false, true] {
        for (primed, built) in [(false, [1, 0]), (true, [0, 1])] {
            let build = || root_facts_fixture(&source, background, primed);
            let predicted = root_facts_prediction(&build(), primed);
            assert_facts_admitted_in_full(
                site,
                &build,
                &|fixture: &Fixture| submitted(site, fixture, built),
                REPLY_RESERVATION,
                predicted,
                &[Trace::Reserved(REPLY_RESERVATION)],
                NO_LOADS,
            );
            for headroom in [0, predicted - 1] {
                assert_refused_reply(site, &build(), REPLY_RESERVATION + headroom, 0);
            }
        }
    }
}

fn warm_root_facts_fixture(source: &LedgerSource) -> Fixture {
    let store = prepared_store();
    let owner = context_for("warm-root-facts", true);
    let (_, snapshot) = acquired(&store, &source.path, &owner);
    let (spare, _) = acquired(&store, &source.path, &owner);
    release_lease(&store, &spare, &owner);
    Fixture {
        store,
        owner,
        request: warm_root_request(&source.path),
        pins: vec![snapshot],
    }
}

#[test]
fn warm_root_refuses_root_facts_before_construction() {
    let source = LedgerSource::new(&prompt_line("root", 32 * 1024));
    let site = "warm_root root facts";
    let build = || warm_root_facts_fixture(&source);
    let probe = build();
    assert_eq!(
        settled(&probe.store)[ENTRIES],
        entry_bytes(&[&probe.pins[0]])
    );
    let predicted = facts_bound_walk(&probe.pins[0]);
    let held = load_slot_ids(&probe.store);
    let tier = probe.store.lock_state().loads.reserved_bytes();
    assert!(submitted(site, &probe, [1, 0]));
    let (slots, slot_bytes) = residue_walk(&probe.store, &held);
    assert_eq!(
        slots, 1,
        "{site}: the root acquire did not cache one load slot"
    );
    let offset =
        REPLY_RESERVATION + slot_bytes + probe.store.lock_state().loads.reserved_bytes() - tier;
    assert_facts_admitted_in_full(
        site,
        &build,
        &|fixture: &Fixture| submitted(site, fixture, [1, 0]),
        offset,
        predicted,
        &[Trace::Reserved(REPLY_RESERVATION)],
        UNPINNED_LOAD,
    );
    assert_refused_reply(site, &build(), offset + predicted - 1, 1);
}

#[test]
fn source_facts_disk_hit_is_admitted_in_full_before_decoding() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let path = &scenario.sidechains[0];
    let site = "prepared_source disk hit";
    for background in [false, true] {
        let build = || facts_cache_fixture(&scenario, background, 0);
        let predicted = decoded_source_facts_prediction(&build());
        let attempt = |fixture: &Fixture| {
            let mut usage = [0u64; 18];
            let fitted = constructed(site, &fixture.store, [0, 1], false, || {
                let (_, outcome) = fixture.store.prepared_source(
                    path,
                    &fixture.owner,
                    &mut work_bounds(),
                    &Cancellation::default(),
                    &mut usage,
                )?;
                assert!(matches!(
                    outcome,
                    PreparedSourceOutcome::Ready { cached: false, .. }
                ));
                Ok(())
            });
            let mut expected = [0u64; 18];
            expected[7] = u64::from(fitted);
            assert_eq!(usage, expected, "{site}: usage misreported the attempt");
            fitted
        };
        assert_facts_admitted_in_full(site, &build, &attempt, 0, predicted, &[], NO_LOADS);
    }
}

#[test]
fn query_graph_refuses_a_source_disk_hit_before_decoding() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "query_graph source disk hit";
    for background in [false, true] {
        let build = || facts_cache_fixture(&scenario, background, 0);
        let predicted = decoded_source_facts_prediction(&build());
        let record = prepared_query_record_bytes(&build());
        assert_facts_admitted_in_full(
            site,
            &build,
            &|fixture: &Fixture| submitted(site, fixture, [0, 1]),
            REPLY_RESERVATION + record,
            predicted,
            &[Trace::Reserved(REPLY_RESERVATION), Trace::Reserved(record)],
            NO_LOADS,
        );
        for headroom in [0, predicted - 1] {
            assert_refused_reply(site, &build(), REPLY_RESERVATION + record + headroom, 0);
        }
    }
}

fn finish_fixture(store: NativeStore, path: &Path, background: bool) -> Fixture {
    let owner = context_for("finish", background);
    let mut request = acquire(path);
    let acquired = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    let snapshot = store.pin(&ok_handle(&acquired), &owner).unwrap();
    request.insert("outcome", acquired["data"].clone());
    Fixture {
        store,
        owner,
        request,
        pins: vec![snapshot],
    }
}

fn finish_source(fixture: &Fixture) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
    fixture.store.finish_prepared_source(
        SourceStamp::of(&std::fs::metadata(fixture.request["path"].as_str().unwrap()).unwrap()),
        &fixture.request["outcome"],
        &fixture.owner,
        &work_bounds(),
        &Cancellation::default(),
    )
}

fn lease_id(fixture: &Fixture) -> String {
    fixture.request["outcome"]["description"]["handle"]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn finished_source_facts_are_admitted_in_full_before_construction() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "finish_prepared_source";
    for background in [false, true] {
        let build = || finish_fixture(prepared_store(), &scenario.sidechains[0], background);
        let probe = build();
        assert_eq!(
            settled(&probe.store)[ENTRIES],
            entry_bytes(&[&probe.pins[0]])
        );
        assert_completion_admitted_in_full(site, &build, facts_bound_walk(&probe.pins[0]));
    }
}

fn assert_completion_admitted_in_full(site: &str, build: &dyn Fn() -> Fixture, predicted: usize) {
    let attempt = |fixture: &Fixture| {
        constructed(site, &fixture.store, [1, 0], false, || {
            finish_source(fixture).map(|_| ())
        })
    };
    assert_eq!(
        exact_headroom(build, &attempt),
        predicted,
        "{site}: completion is not gated on its full {predicted}-byte bound"
    );
    for headroom in [0, predicted - 1] {
        let control = build();
        let refused = build();
        let identity = refused.pins[0].stamp.identity;
        let _control_filler = fill_to(&control.store, &control.owner, headroom);
        control
            .store
            .lock_state()
            .leases
            .remove(&lease_id(&control));
        let _filler = fill_to(&refused.store, &refused.owner, headroom);
        refused.store.assert_conserved();
        let (before, probes, disk) = (
            ledger(&refused.store),
            fact_probes(&refused.store),
            refused.store.prepared_disk.stats(),
        );
        traced(&refused.store);
        assert!(
            !attempt(&refused),
            "{site}: headroom {headroom} below the prediction was admitted"
        );
        assert!(
            traced(&refused.store).is_empty(),
            "{site}: the refusal admitted, reserved, or allocated"
        );
        refused.store.assert_conserved();
        assert_eq!(
            ledger(&refused.store)[LEASES],
            before[LEASES] - 1,
            "{site}: the refusal leaked its source lease"
        );
        assert_eq!(
            (
                ledger(&refused.store),
                audited(&refused.store),
                bookkeeping(&refused.store),
                lease_table(&refused.store),
            ),
            (
                ledger(&control.store),
                audited(&control.store),
                bookkeeping(&control.store),
                lease_table(&control.store),
            ),
            "{site}: the refusal changed more than consuming its lease"
        );
        assert_eq!(fact_probes(&refused.store), probes);
        let stats = refused.store.prepared_disk.stats();
        assert_eq!(
            (stats.entries, stats.bytes, stats.writes),
            (disk.entries, disk.bytes, disk.writes)
        );
        let state = refused.store.lock_state();
        assert!(
            state.latest.contains_key(&identity),
            "{site}: a Claude source keeps its parsed snapshot cached"
        );
        assert!(!state.prepared_facts.contains_key(&identity));
        assert!(state.waiters.is_empty());
        assert!(state.prepared_loads.is_empty());
    }
    assert_fitted_at(site, &build(), &attempt, predicted);
}

fn completion_query_fixture(scenario: &Scenario, index: usize, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("completion", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let path = &scenario.sidechains[index];
    let (_, sidechain) = acquired(&store, path, &owner);
    let (spare, _) = acquired(&store, path, &owner);
    release_lease(&store, &spare, &owner);
    let graph = prepared_graph(
        &store,
        &root,
        &[path.to_string_lossy().into_owned()],
        &owner,
    );
    assert!(!store
        .lock_state()
        .prepared_facts
        .contains_key(&sidechain.stamp.identity));
    {
        let mut state = store.lock_state();
        state.prepared_loads.reserve(1);
        state.prepared_loads_expiry.reserve(1);
        assert_eq!(
            state.prepared_load_growth(&sidechain.stamp.identity),
            0,
            "the completion's prepared load pin still grows its tables"
        );
    }
    Fixture {
        store,
        owner,
        request: graph_query(&graph, missing_tool(), json!([])),
        pins: vec![root_snapshot, sidechain],
    }
}

#[test]
fn prepared_query_refuses_source_completion_before_construction() {
    let scenario = Scenario::new(2, |index| match index {
        0 => prompt_line("thread-0000", 8 * 1024),
        _ => prompt_line("thread-0001", 16 * 1024),
    });
    let site = "source completion";
    let attempt = |fixture: &Fixture| submitted(site, fixture, [1, 0]);
    for background in [false, true] {
        let build = |index: usize| completion_query_fixture(&scenario, index, background);
        let (small_bytes, large_bytes) = (
            facts_bound_walk(&build(0).pins[1]),
            facts_bound_walk(&build(1).pins[1]),
        );
        let small = exact_headroom(&|| build(0), &attempt);
        let large = exact_headroom(&|| build(1), &attempt);
        assert_eq!(
            large - small,
            large_bytes - small_bytes,
            "{site}: completion is not gated on its facts bound"
        );
        let offset = large - large_bytes;
        let record = prepared_query_record_bytes(&build(1));
        assert!(
            offset > REPLY_RESERVATION + record,
            "{site}: the internal source lease was not charged before completion"
        );
        assert_facts_admitted_in_full(
            site,
            &|| build(1),
            &attempt,
            offset,
            large_bytes,
            &[Trace::Reserved(REPLY_RESERVATION), Trace::Reserved(record)],
            PINNED_LOAD,
        );
        assert_refused_reply(site, &build(1), large - 1, 1);
    }
}

fn root_slice_prediction(fixture: &Fixture) -> usize {
    facts_bound_walk(&fixture.pins[0])
}

#[test]
fn root_slice_is_admitted_in_full_before_construction() {
    let scenario = Scenario::new(0, |_| String::new());
    let site = "query_graph root slice";
    for background in [false, true] {
        let build = || slice_fixture(&scenario, background);
        let probe = build();
        let predicted = root_slice_prediction(&probe);
        let slice_bytes = NativeStore::audit_facts_bytes(&built_facts(
            &probe.pins[0],
            &json!([{"kind":"current_turn"}]),
        ));
        assert!(
            predicted >= slice_bytes,
            "{site}: the {predicted}-byte reservation does not cover the {slice_bytes}-byte slice"
        );
        let attempt = |fixture: &Fixture| {
            let mut usage = [0u64; 18];
            let fitted = constructed(site, &fixture.store, [1, 0], true, || {
                fixture
                    .store
                    .query_graph(
                        &fixture.request,
                        &fixture.owner,
                        &Cancellation::default(),
                        &mut usage,
                    )
                    .map(|_| ())
            });
            if !fitted {
                assert_eq!(usage, [0u64; 18], "{site}: the refusal reported work");
            }
            fitted
        };
        assert_facts_admitted_in_full(site, &build, &attempt, 0, predicted, &[], NO_LOADS);
    }
}

#[test]
fn query_graph_refuses_a_root_slice_before_construction() {
    let scenario = Scenario::new(0, |_| String::new());
    let site = "query_graph root slice reply";
    for background in [false, true] {
        let build = || slice_fixture(&scenario, background);
        let predicted = root_slice_prediction(&build());
        assert_facts_admitted_in_full(
            site,
            &build,
            &|fixture: &Fixture| submitted(site, fixture, [1, 0]),
            REPLY_RESERVATION,
            predicted,
            &[Trace::Reserved(REPLY_RESERVATION)],
            NO_LOADS,
        );
        for headroom in [0, predicted - 1] {
            assert_refused_reply(site, &build(), REPLY_RESERVATION + headroom, 0);
        }
    }
}

fn patch_paths(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("pkg/module/file-{index:04}.rs"))
        .collect()
}

fn delete_patch(paths: &[String]) -> String {
    format!(
        "*** Begin Patch\n{}*** End Patch\n",
        paths
            .iter()
            .map(|path| format!("*** Delete File: {path}\n"))
            .collect::<String>()
    )
}

fn multi_path_patch(paths: &[String]) -> String {
    format!(
        "*** Begin Patch\n{}*** End Patch\n",
        paths
            .iter()
            .enumerate()
            .map(|(index, path)| match index % 3 {
                0 => format!("*** Update File: {path}\n*** Move to: moved/{path}\n@@\n-a\n+b\n"),
                1 => format!("*** Add File: {path}\n+x\n"),
                _ => format!("*** Delete File: {path}\n"),
            })
            .collect::<String>()
    )
}

fn patch_shapes() -> [(&'static str, String, Vec<String>); 2] {
    let (deleted, mixed) = (patch_paths(200), patch_paths(150));
    [
        ("delete-only", delete_patch(&deleted), deleted),
        ("multi-path", multi_path_patch(&mixed), mixed),
    ]
}

fn patch_transcript(patch: &str, failed: bool) -> String {
    [
        json!({"type":"user","uuid":"prompt","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":[{"type":"text","text":"apply the patch"},{"type":"tool_result","tool_use_id":"earlier","content":"Done!"}]}}),
        json!({"type":"assistant","uuid":"patch","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"text","text":"Applying"},{"type":"text","text":"it."},{"type":"tool_use","id":"patch-call","name":"apply_patch","input":patch}]}}),
        json!({"type":"user","uuid":"patch-result","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","message":{"content":[{"type":"tool_result","tool_use_id":"patch-call","content":"applied","is_error":failed}]}}),
    ]
    .map(|entry| format!("{entry}\n"))
    .concat()
}

fn assert_paths_twice(site: &str, facts: &PreparedFacts, paths: &[String]) {
    let listed: Vec<&str> = facts.inputs["calls"][0][1]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    let edited: Vec<&str> = facts.inputs["edited_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        facts.inputs["calls"][0][0].as_str(),
        Some("apply_patch"),
        "{site}"
    );
    assert_eq!(
        listed, paths,
        "{site}: calls[0] does not list every patched path"
    );
    assert_eq!(
        edited, paths,
        "{site}: edited_files does not repeat every patched path"
    );
}

fn decoded_slice_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("decoded-slices", background);
    let (root, snapshot) = acquired(&store, &source.path, &owner);
    prepared_graph(&store, &root, &[], &owner);
    store
        .lock_state()
        .remove_prepared_facts(&snapshot.stamp.identity)
        .expect("built root facts");
    let probes = fact_probes(&store);
    let graph = prepared_graph(&store, &root, &[], &owner);
    assert_eq!(
        fact_probes(&store),
        [probes[BUILDS], probes[LOOKUPS] + 1],
        "the second graph did not decode its root facts from disk"
    );
    Fixture {
        store,
        owner,
        request: graph_query(
            &graph,
            json!({"kind":"has_read","pattern":"missing","subagents":true}),
            json!([{"kind":"current_turn"}]),
        ),
        pins: vec![snapshot],
    }
}

#[test]
fn patch_facts_outgrow_their_entry_bytes_but_never_their_bound() {
    let store = prepared_store();
    let owner = context_for("patch-facts", false);
    for (shape, patch, paths) in patch_shapes() {
        for failed in [false, true] {
            let site = format!("{shape} failed={failed}");
            let source = LedgerSource::new(&patch_transcript(&patch, failed));
            let (handle, snapshot) = acquired(&store, &source.path, &owner);
            let full = built_facts(&snapshot, &json!([]));
            let walked = NativeStore::audit_facts_bytes(&full);
            let bound = facts_bound_walk(&snapshot);
            assert_eq!(
                bound,
                crate::snapshot_projection::facts_bound(&snapshot),
                "{site}: the production bound departs from the independent bound"
            );
            assert_eq!(
                walked,
                full.accounted_bytes(),
                "{site}: the facts misreport their DOM"
            );
            if failed {
                assert!(
                    full.inputs["edited_files"].as_array().unwrap().is_empty(),
                    "{site}: a failed patch reached the edited files"
                );
                assert!(walked < bound, "{site}: walked={walked} bound={bound}");
            } else {
                assert_paths_twice(&site, &full, &paths);
                let entries = chunk_entry_bytes(&snapshot);
                assert!(
                    entries < walked,
                    "{site}: the {entries}-byte entry predictor already covers the {walked}-byte facts"
                );
                assert_eq!(
                    bound, walked,
                    "{site}: the bound is not exact on a clean root"
                );
            }
            for selectors in [
                json!([{"kind":"current_turn"}]),
                json!([{"kind":"recent_events","count":2}]),
                json!([{"kind":"event_range","start":1,"stop":2}]),
                json!([{"kind":"after_last_tool","name":"apply_patch"}]),
                json!([{"kind":"before_last_tool","name":"apply_patch"}]),
                json!([{"kind":"prior"}]),
            ] {
                let slice = NativeStore::audit_facts_bytes(&built_facts(&snapshot, &selectors));
                assert!(
                    slice <= bound,
                    "{site} {selectors}: the {slice}-byte slice exceeds the {bound}-byte bound"
                );
            }
            release_lease(&store, &handle, &owner);
        }
    }
}

#[test]
fn patch_root_facts_are_admitted_at_their_bound_before_construction() {
    for (shape, patch, paths) in patch_shapes() {
        let source = LedgerSource::new(&patch_transcript(&patch, false));
        let direct = format!("{shape} prepared_root_facts");
        let graph = format!("{shape} prepare_graph root facts");
        for background in [false, true] {
            let build = || root_facts_fixture(&source, background, false);
            let probe = build();
            let facts = built_facts(&probe.pins[0], &json!([]));
            assert_paths_twice(&direct, &facts, &paths);
            let predicted = facts_bound_walk(&probe.pins[0]);
            assert_eq!(
                predicted,
                NativeStore::audit_facts_bytes(&facts),
                "{shape}: the bound is not exact on a clean root"
            );
            assert!(
                chunk_entry_bytes(&probe.pins[0]) < predicted,
                "{shape}: the fixture does not defeat the entry-bytes predictor"
            );
            assert_facts_admitted_in_full(
                &direct,
                &build,
                &|fixture: &Fixture| {
                    constructed(&direct, &fixture.store, [1, 0], false, || {
                        fixture
                            .store
                            .prepared_root_facts(
                                &fixture.pins[0],
                                &json!({"id":"native","version":"1"}),
                                &fixture.owner,
                                &work_bounds(),
                                &Cancellation::default(),
                            )
                            .map(|_| ())
                    })
                },
                0,
                predicted,
                &[],
                NO_LOADS,
            );
            assert_facts_admitted_in_full(
                &graph,
                &build,
                &|fixture: &Fixture| submitted(&graph, fixture, [1, 0]),
                REPLY_RESERVATION,
                predicted,
                &[Trace::Reserved(REPLY_RESERVATION)],
                NO_LOADS,
            );
            for headroom in [0, predicted - 1] {
                assert_refused_reply(&graph, &build(), REPLY_RESERVATION + headroom, 0);
            }
        }
    }
}

#[test]
fn patch_source_completion_is_admitted_at_its_bound_before_construction() {
    for (shape, patch, paths) in patch_shapes() {
        let source = LedgerSource::new(&patch_transcript(&patch, false));
        let site = format!("{shape} finish_prepared_source");
        for background in [false, true] {
            let build = || finish_fixture(prepared_store(), &source.path, background);
            let probe = build();
            let facts = built_facts(&probe.pins[0], &json!([]));
            assert_paths_twice(&site, &facts, &paths);
            let predicted = facts_bound_walk(&probe.pins[0]);
            assert_eq!(
                predicted,
                NativeStore::audit_facts_bytes(&facts),
                "{shape}: the bound is not exact on a clean root"
            );
            assert!(
                chunk_entry_bytes(&probe.pins[0]) < predicted,
                "{shape}: the fixture does not defeat the entry-bytes predictor"
            );
            assert_completion_admitted_in_full(&site, &build, predicted);
        }
    }
}

#[test]
fn slices_of_a_decoded_patch_root_are_admitted_at_the_root_bound() {
    for (shape, patch, _) in patch_shapes() {
        let source = LedgerSource::new(&patch_transcript(&patch, false));
        let site = format!("{shape} decoded root slice");
        for background in [false, true] {
            let build = || decoded_slice_fixture(&source, background);
            let probe = build();
            let graph_id = probe.request["handle"]["graph_id"].as_str().unwrap();
            let decoded = NativeStore::audit_facts_bytes(
                &probe.store.lock_state().prepared_graphs[graph_id]
                    .lock()
                    .unwrap()
                    .root_facts,
            );
            let slice = NativeStore::audit_facts_bytes(&built_facts(
                &probe.pins[0],
                &json!([{"kind":"current_turn"}]),
            ));
            let predicted = facts_bound_walk(&probe.pins[0]);
            assert!(
                decoded < slice,
                "{site}: the {decoded}-byte decoded root facts already bound the {slice}-byte slice"
            );
            assert!(
                slice <= predicted,
                "{site}: slice={slice} predicted={predicted}"
            );
            let attempt = |fixture: &Fixture| {
                let mut usage = [0u64; 18];
                let fitted = constructed(&site, &fixture.store, [1, 0], true, || {
                    fixture
                        .store
                        .query_graph(
                            &fixture.request,
                            &fixture.owner,
                            &Cancellation::default(),
                            &mut usage,
                        )
                        .map(|_| ())
                });
                if !fitted {
                    assert_eq!(usage, [0u64; 18], "{site}: the refusal reported work");
                }
                fitted
            };
            assert_facts_admitted_in_full(&site, &build, &attempt, 0, predicted, &[], NO_LOADS);
        }
    }
}

fn degraded_root_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = store_with(
        64 * 1024 * 1024,
        4096,
        &[("max_retained_bytes", 8 * CAP), ("max_entry_bytes", CAP)],
    );
    let owner = context_for("degraded-slices", background);
    let mut request = acquire(&source.path);
    for limit in ["max_read_bytes", "max_source_read_bytes"] {
        request["limits"].insert(limit, json!(CAP));
    }
    let handle = ok_handle(&drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    ));
    let root_snapshot = store.pin(&handle, &owner).unwrap();
    let graph = prepared_graph(&store, &handle, &[], &owner);
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

#[test]
fn degraded_root_slices_are_admitted_at_the_root_bound_before_construction() {
    let source = LedgerSource::new(&format!(
        "{}{}{}",
        prompt_line("early", 17 * 1024 * 1024),
        tool_line("reply", "Bash"),
        prompt_line("now", 256 * 1024)
    ));
    let site = "query_graph degraded root slice";
    for background in [false, true] {
        let build = || degraded_root_fixture(&source, background);
        let probe = build();
        let cap = cap_of(&probe.store, &probe.owner);
        let graph_id = probe.request["handle"]["graph_id"].as_str().unwrap();
        assert!(
            probe.store.lock_state().prepared_graphs[graph_id]
                .lock()
                .unwrap()
                .root_facts
                .override_events
                .is_none(),
            "{site}: the full root facts kept their overrides"
        );
        let slice = built_facts(&probe.pins[0], &json!([{"kind":"current_turn"}]));
        let overrides = slice
            .override_events
            .as_ref()
            .expect("the current turn keeps its overrides");
        assert_eq!(overrides.len(), 1, "{site}: the current turn is one event");
        assert_eq!(overrides[0].text.len(), 256 * 1024);
        let predicted = facts_bound_walk(&probe.pins[0]);
        let slice_bytes = NativeStore::audit_facts_bytes(&slice);
        assert!(
            predicted >= slice_bytes,
            "{site}: the {predicted}-byte reservation does not cover the {slice_bytes}-byte slice"
        );
        let attempt = |fixture: &Fixture| {
            let mut usage = [0u64; 18];
            constructed(site, &fixture.store, [1, 0], true, || {
                fixture
                    .store
                    .query_graph(
                        &fixture.request,
                        &fixture.owner,
                        &Cancellation::default(),
                        &mut usage,
                    )
                    .map(|_| ())
            })
        };
        for headroom in [0, predicted - 1] {
            let fixture = build();
            let filler = fill_to(&fixture.store, &fixture.owner, headroom);
            fixture.store.assert_conserved();
            let before = (
                ledger(&fixture.store),
                audited(&fixture.store),
                bookkeeping(&fixture.store),
            );
            traced(&fixture.store);
            assert!(
                !attempt(&fixture),
                "{site}: headroom {headroom} below the prediction was admitted"
            );
            let trace = traced(&fixture.store);
            assert!(
                !trace
                    .iter()
                    .any(|entry| matches!(entry, Trace::Reserved(_) | Trace::Allocated(_))),
                "{site}: the refusal at headroom {headroom} reserved or allocated: {trace:?}"
            );
            fixture.store.assert_conserved();
            assert_eq!(
                (
                    ledger(&fixture.store),
                    audited(&fixture.store),
                    bookkeeping(&fixture.store),
                ),
                before,
                "{site}: the refusal at headroom {headroom} leaked state"
            );
            assert!(audited(&fixture.store)[TOTAL] <= cap);
            drop(filler);
            assert_eq!(fixture.store.lock_state().transient_bytes, 0);
        }
        let fitted = build();
        let _filler = fill_to(&fitted.store, &fitted.owner, predicted);
        assert!(attempt(&fitted), "{site}: the exact fit was refused");
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        let graph_id = fitted.request["handle"]["graph_id"].as_str().unwrap();
        let state = fitted.store.lock_state();
        let graph = state.prepared_graphs[graph_id].lock().unwrap();
        assert_eq!(
            graph.root_slices.len(),
            1,
            "{site}: the slice was not published"
        );
        assert!(graph
            .root_slices
            .values()
            .all(|published| published.accounted_bytes() <= predicted));
    }
}

#[test]
fn finished_source_facts_hold_their_reservation_until_publication() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "finish_prepared_source barrier";
    for background in [false, true] {
        let control = finish_fixture(prepared_store(), &scenario.sidechains[0], background);
        let leased = ledger(&control.store)[TOTAL];
        control
            .store
            .lock_state()
            .leases
            .remove(&lease_id(&control));
        let lease_bytes = leased - ledger(&control.store)[TOTAL];
        assert!(lease_bytes > 0, "{site}: the source lease is not charged");
        let fixture = finish_fixture(prepared_store(), &scenario.sidechains[0], background);
        let cap = cap_for(&fixture.owner);
        let predicted = facts_bound_walk(&fixture.pins[0]);
        let occupied = NativeStore::audit_facts_bytes(&built_facts(&fixture.pins[0], &json!([])));
        assert!(
            occupied <= predicted,
            "{site}: the {predicted}-byte prediction does not cover the {occupied}-byte facts"
        );
        let (paused_tx, paused_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        *fixture.store.built_facts_hook.lock().unwrap() =
            Some(Arc::new(move |_: &Arc<PreparedFacts>| {
                paused_tx.send(()).unwrap();
                resume_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(DROP_TIMEOUT)
                    .expect("the barrier was never released");
            }));
        let filler = fill_to(&fixture.store, &fixture.owner, predicted);
        std::thread::scope(|scope| {
            let finishing = scope.spawn(|| finish_source(&fixture));
            let paused = paused_rx.recv_timeout(DROP_TIMEOUT);
            let observed = paused.is_ok().then(|| {
                let held = ledger(&fixture.store)[TOTAL];
                let audit = audited(&fixture.store)[TOTAL];
                let plug = fixture
                    .store
                    .reserve_projection(&fixture.owner, lease_bytes);
                let refused = fixture
                    .store
                    .reserve_projection(&fixture.owner, occupied)
                    .err();
                let after = ledger(&fixture.store)[TOTAL];
                drop(plug);
                (held, audit, refused, after)
            });
            resume_tx.send(()).unwrap();
            let outcome = finishing.join().unwrap();
            paused.expect("the facts were never built");
            let (held, audit, refused, after) = observed.unwrap();
            assert_eq!(
                held,
                cap - lease_bytes,
                "{site}: the reservation is not held while the facts are live"
            );
            assert!(audit <= cap);
            let refused = refused.expect("the headroom the facts occupy was admitted twice");
            assert_eq!(refused.status, Status::RetainedLimit, "{site}: {refused:?}");
            assert_eq!(after, cap, "{site}: the refused admission moved the ledger");
            assert!(
                matches!(
                    outcome,
                    Ok(PreparedSourceOutcome::Ready { cached: false, .. })
                ),
                "{site}: the finish did not publish its facts"
            );
        });
        fixture.store.assert_conserved();
        assert!(audited(&fixture.store)[TOTAL] <= cap);
        drop(filler);
        assert_eq!(fixture.store.lock_state().transient_bytes, 0);
        fixture.store.assert_conserved();
    }
}

fn disk_refusing_store() -> NativeStore {
    store_with(
        4096,
        2048,
        &[(
            "max_prepared_disk_bytes",
            crate::snapshot_prepared_disk::HEADER_BYTES + 1,
        )],
    )
}

#[test]
fn finished_source_facts_are_freed_before_their_reservation_when_the_disk_write_fails() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "finish_prepared_source disk write failure";
    for background in [false, true] {
        let fixture = finish_fixture(disk_refusing_store(), &scenario.sidechains[0], background);
        let built: Arc<Mutex<Option<Weak<PreparedFacts>>>> = Arc::new(Mutex::new(None));
        *fixture.store.built_facts_hook.lock().unwrap() = Some(Arc::new({
            let built = Arc::clone(&built);
            move |facts: &Arc<PreparedFacts>| {
                *built.lock().unwrap() = Some(Arc::downgrade(facts));
            }
        }));
        let releases = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        *fixture.store.release_hook.lock().unwrap() = Some(Arc::new({
            let (built, releases, live) =
                (Arc::clone(&built), Arc::clone(&releases), Arc::clone(&live));
            move |_: usize| {
                if let Some(facts) = built.lock().unwrap().as_ref() {
                    releases.fetch_add(1, Ordering::Relaxed);
                    live.fetch_add(usize::from(facts.upgrade().is_some()), Ordering::Relaxed);
                }
            }
        }));
        let outcome = finish_source(&fixture);
        *fixture.store.release_hook.lock().unwrap() = None;
        let error = outcome
            .err()
            .expect("the capacity-exhausted disk write landed");
        assert_eq!(error.status, Status::Incomplete, "{site}: {error:?}");
        assert!(
            built.lock().unwrap().is_some(),
            "{site}: the facts were never built"
        );
        assert_eq!(
            releases.load(Ordering::Relaxed),
            1,
            "{site}: the facts reservation was not released exactly once"
        );
        assert_eq!(
            live.load(Ordering::Relaxed),
            0,
            "{site}: the reservation was released while the facts were live"
        );
        assert_eq!(fixture.store.lock_state().transient_bytes, 0);
        fixture.store.assert_conserved();
    }
}

fn uncached_facts_store() -> NativeStore {
    store_with(4096, 2048, &[("max_prepared_fact_memory_bytes", 1)])
}

fn uncached_root_facts_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = uncached_facts_store();
    let owner = context_for("returned-root-facts", background);
    let (root, snapshot) = acquired(&store, &source.path, &owner);
    Fixture {
        store,
        owner,
        request: prepare_request(&root, &[], &[], &[]),
        pins: vec![snapshot],
    }
}

fn uncached_source_facts_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = uncached_facts_store();
    let owner = context_for("returned-source-facts", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let (_, sidechain) = acquired(&store, &scenario.sidechains[0], &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    let request = graph_query(&graph, missing_tool(), json!([]));
    let primed = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(primed["status"].as_str(), Some("ok"), "{primed:?}");
    assert_eq!(
        store.lock_state().prepared_facts.len(),
        0,
        "the one-byte facts budget admitted a cache write"
    );
    assert!(store
        .prepared_disk
        .has_entry(&disk_key(&owner, sidechain.stamp))
        .unwrap());
    Fixture {
        store,
        owner,
        request,
        pins: vec![root_snapshot, sidechain],
    }
}

fn returned_facts_barrier<T>(fixture: &Fixture, during: impl FnOnce() -> T) -> (Value, T) {
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    *fixture.store.returned_facts_hook.lock().unwrap() = Some(Arc::new(move || {
        paused_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(DROP_TIMEOUT)
            .expect("the barrier was never released");
    }));
    std::thread::scope(|scope| {
        let holding = scope.spawn(|| submit(fixture));
        let paused = paused_rx.recv_timeout(DROP_TIMEOUT);
        let observed = paused.is_ok().then(during);
        resume_tx.send(()).unwrap();
        let response = holding.join().unwrap();
        paused.expect("the facts were never returned");
        (response, observed.unwrap())
    })
}

fn held_facts_barrier(
    site: &str,
    fixture: &Fixture,
    filler: ProjectionReservation<'_>,
    occupied: usize,
) -> Value {
    let cap = cap_for(&fixture.owner);
    let (response, (held, audit, refused, after)) = returned_facts_barrier(fixture, || {
        let held = ledger(&fixture.store)[TOTAL];
        let audit = audited(&fixture.store)[TOTAL];
        let refused = fixture
            .store
            .reserve_projection(&fixture.owner, occupied)
            .err();
        let after = ledger(&fixture.store)[TOTAL];
        drop(filler);
        (held, audit, refused, after)
    });
    assert_eq!(
        held, cap,
        "{site}: the reservation is not held while the returned facts are live"
    );
    assert!(
        audit <= cap,
        "{site}: the audit exceeded the cap while the facts were held"
    );
    let refused = refused.expect("the headroom the returned facts occupy was admitted twice");
    assert_eq!(refused.status, Status::RetainedLimit, "{site}: {refused:?}");
    assert_eq!(after, cap, "{site}: the refused admission moved the ledger");
    response
}

#[test]
fn uncached_root_facts_hold_their_reservation_until_retention() {
    let source = LedgerSource::new(&lines(0..4));
    let site = "prepare_graph returned facts barrier";
    for background in [false, true] {
        let fixture = uncached_root_facts_fixture(&source, background);
        let cap = cap_for(&fixture.owner);
        let predicted = facts_bound_walk(&fixture.pins[0]);
        let occupied = NativeStore::audit_facts_bytes(&built_facts(&fixture.pins[0], &json!([])));
        assert!(
            occupied <= predicted,
            "{site}: the {predicted}-byte prediction does not cover the {occupied}-byte facts"
        );
        let probes = fact_probes(&fixture.store);
        let filler = fill_to(
            &fixture.store,
            &fixture.owner,
            REPLY_RESERVATION + predicted,
        );
        let response = held_facts_barrier(site, &fixture, filler, occupied);
        assert_eq!(
            response["status"].as_str(),
            Some("ok"),
            "{site}: {response:?}"
        );
        assert_eq!(
            response["data"]["kind"].as_str(),
            Some("prepared_graph"),
            "{site}: {response:?}"
        );
        assert_eq!(
            fact_probes(&fixture.store),
            [probes[BUILDS] + 1, probes[LOOKUPS]],
            "{site}: the build did not construct its root facts exactly once"
        );
        fixture.store.assert_conserved();
        assert!(audited(&fixture.store)[TOTAL] <= cap);
        assert_eq!(fixture.store.lock_state().transient_bytes, 0);
    }
}

#[test]
fn uncached_source_facts_hold_their_reservation_until_consumption() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "prepared_query_page returned facts barrier";
    for background in [false, true] {
        let fixture = uncached_source_facts_fixture(&scenario, background);
        let cap = cap_for(&fixture.owner);
        let predicted = decoded_source_facts_prediction(&fixture);
        let occupied = NativeStore::audit_facts_bytes(&built_facts(&fixture.pins[1], &json!([])));
        let probes = fact_probes(&fixture.store);
        let filler = fill_to(
            &fixture.store,
            &fixture.owner,
            REPLY_RESERVATION + predicted,
        );
        let response = held_facts_barrier(site, &fixture, filler, occupied);
        assert_eq!(
            response["status"].as_str(),
            Some("ok"),
            "{site}: {response:?}"
        );
        assert_eq!(
            response["data"],
            json!({"kind":"scalar","value":false}),
            "{site}: {response:?}"
        );
        assert_eq!(
            fact_probes(&fixture.store),
            [probes[BUILDS], probes[LOOKUPS] + 1],
            "{site}: the page did not decode its source facts exactly once"
        );
        fixture.store.assert_conserved();
        assert!(audited(&fixture.store)[TOTAL] <= cap);
        assert_eq!(fixture.store.lock_state().transient_bytes, 0);
    }
}

#[test]
fn user_text_with_tool_results_is_built_at_exact_capacity() {
    let (text, result) = ("t".repeat(64), "r".repeat(8));
    let source = LedgerSource::new(&format!(
        "{}\n",
        json!({"type":"user","uuid":"mixed","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":[{"type":"text","text":text},{"type":"tool_result","tool_use_id":"call","content":result}]}})
    ));
    let store = fast_store();
    let owner = context_for("exact-text", false);
    let (_, snapshot) = acquired(&store, &source.path, &owner);
    let facts = built_facts(&snapshot, &json!([]));
    let events = facts
        .override_events
        .as_ref()
        .expect("bounded override facts");
    assert_eq!(events.len(), 1);
    let joined = &events[0].text;
    assert_eq!(*joined, format!("{text}{result}"));
    assert_eq!(joined.capacity(), joined.len());
    assert_eq!(events[0].tools.capacity(), 0);
    assert_eq!(
        NativeStore::audit_facts_bytes(&facts),
        arc_mirror::<PreparedFacts>()
            + NativeStore::audit_value_bytes(&facts.inputs)
            + events.capacity() * size_of::<OverrideEvent>()
            + text.len()
            + result.len()
    );
    assert_eq!(
        NativeStore::audit_facts_bytes(&facts),
        facts.accounted_bytes()
    );
}

fn primed_disk_index_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("disk-index", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    let primed = drive(
        &store,
        store.request(
            &graph_query(&graph, missing_tool(), json!([])),
            &owner,
            &Cancellation::default(),
        ),
        &owner,
    );
    assert_eq!(primed["status"].as_str(), Some("ok"), "{primed:?}");
    assert_eq!(
        store.prepared_disk.stats().entries,
        scenario.sidechains.len() + 1
    );
    let spare = scenario.root.file("spare.jsonl", &line("spare"));
    let (_, spare_snapshot) = acquired(&store, &spare, &owner);
    let pins = [root_snapshot, spare_snapshot]
        .into_iter()
        .chain(scenario.sidechains.iter().map(|path| {
            let (handle, snapshot) = acquired(&store, path, &owner);
            release_lease(&store, &handle, &owner);
            snapshot
        }))
        .collect();
    Fixture {
        store,
        owner,
        request: json!(null),
        pins,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Landing {
    Landed,
    Returned,
    Refused,
}

#[test]
fn prepared_disk_index_growth_is_admitted_before_the_index_grows() {
    let scenario = Scenario::new(111, |index| line(&format!("thread-{index:04}")));
    let site = "prepared disk index growth";
    let attempt = |fixture: &Fixture| {
        let before = fixture.store.prepared_disk.stats().entries;
        match fixture.store.prepared_root_facts(
            &fixture.pins[1],
            &json!({"id":"native","version":"1"}),
            &fixture.owner,
            &work_bounds(),
            &Cancellation::default(),
        ) {
            Ok(_) if fixture.store.prepared_disk.stats().entries > before => Landing::Landed,
            Ok(_) => Landing::Returned,
            Err(error) if error.status == Status::RetainedLimit => Landing::Refused,
            Err(error) => panic!("{site}: failed outside admission: {error:?}"),
        }
    };
    let landed = |fixture: &Fixture| attempt(fixture) == Landing::Landed;
    for background in [false, true] {
        let build = || primed_disk_index_fixture(&scenario, background);
        let control = build();
        let cap = cap_for(&control.owner);
        let bound = facts_bound_walk(&control.pins[1]);
        let payload = sonic_rs::to_vec(&built_facts(&control.pins[1], &json!([])))
            .unwrap()
            .len();
        let (tier, charged) = (
            control.store.prepared_disk.index_capacity_bytes(),
            control.store.lock_state().prepared_disk_index_bytes,
        );
        assert_eq!(
            tier, charged,
            "{site}: the primed index is not charged at its tier"
        );
        traced(&control.store);
        assert!(landed(&control), "{site}: the control insert did not land");
        let growth = control.store.prepared_disk.index_capacity_bytes() - tier;
        assert!(
            growth > 0,
            "{site}: the control insert did not grow the index"
        );
        assert_eq!(
            control.store.lock_state().prepared_disk_index_bytes - charged,
            growth,
            "{site}: the index growth is not charged to the ledger"
        );
        assert_admitted_before_allocating(site, &traced(&control.store));
        control.store.assert_conserved();
        assert_eq!(
            control
                .store
                .prepared_disk
                .entry_file_len(&disk_key(&control.owner, control.pins[1].stamp))
                as usize,
            crate::snapshot_prepared_disk::HEADER_BYTES + payload,
            "{site}: the control insert did not write the counted payload"
        );
        for (headroom, expected) in [
            (0, Landing::Refused),
            (bound + payload - 1, Landing::Returned),
            (bound + payload + growth - 1, Landing::Returned),
        ] {
            let refused = build();
            let (stats, probes, before) = (
                refused.store.prepared_disk.stats(),
                fact_probes(&refused.store),
                (
                    refused.store.prepared_disk.index_capacity_bytes(),
                    refused.store.lock_state().prepared_disk_index_bytes,
                ),
            );
            let filler = fill_to(&refused.store, &refused.owner, headroom);
            assert_eq!(
                attempt(&refused),
                expected,
                "{site}: headroom {headroom} below the growth changed the outcome"
            );
            let after = refused.store.prepared_disk.stats();
            assert_eq!(
                (after.entries, after.bytes, after.writes),
                (stats.entries, stats.bytes, stats.writes),
                "{site}: the refusal at headroom {headroom} grew the disk cache"
            );
            assert_eq!(
                (
                    refused.store.prepared_disk.index_capacity_bytes(),
                    refused.store.lock_state().prepared_disk_index_bytes,
                ),
                before,
                "{site}: the refusal at headroom {headroom} grew the index"
            );
            assert_eq!(
                fact_probes(&refused.store)[BUILDS] - probes[BUILDS],
                usize::from(headroom >= bound),
                "{site}: unexpected construction at headroom {headroom}"
            );
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap);
            drop(filler);
            assert_eq!(
                refused.store.lock_state().transient_bytes,
                0,
                "{site}: the refusal at headroom {headroom} left a reservation behind"
            );
        }
        let fitted = build();
        let _filler = fill_to(&fitted.store, &fitted.owner, bound + payload + growth);
        assert!(landed(&fitted), "{site}: the exact growth was refused");
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
    }
}

fn build_probes(store: &NativeStore) -> [usize; 2] {
    [
        store.build_records.load(Ordering::Relaxed),
        store.build_sources.load(Ordering::Relaxed),
    ]
}

fn reserved_traces(trace: &[Trace]) -> Vec<usize> {
    trace
        .iter()
        .filter_map(|entry| match entry {
            Trace::Reserved(bytes) => Some(*bytes),
            _ => None,
        })
        .collect()
}

fn build_site(site: &str, probe: usize) -> impl Fn(&Fixture) -> bool + '_ {
    move |fixture: &Fixture| {
        let before = build_probes(&fixture.store);
        let response = submit(fixture);
        match build_probes(&fixture.store)[probe] - before[probe] {
            0 => {
                assert_refusal_contract(site, &response);
                assert_eq!(
                    response["usage"]["discovery_entries_examined"].as_u64(),
                    Some(0),
                    "{site}: {response:?}"
                );
                false
            }
            1 => true,
            moved => panic!("{site}: one attempt crossed the site {moved} times: {response:?}"),
        }
    }
}

fn parked_build_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("resumed-build", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let first = store.request(
        &prepare_request(&root, &[], &[], &scenario.direct()),
        &owner,
        &Cancellation::default(),
    );
    assert!(
        parked(&first),
        "the build finished in its first step: {first:?}"
    );
    assert_eq!(store.lock_state().prepared_builds.len(), 1);
    Fixture {
        store,
        owner,
        request: resume_request(first["cursor"].as_str().unwrap()),
        pins: vec![root_snapshot],
    }
}

fn grown_by_one<T: Clone>(vec: &Vec<T>) -> usize {
    let mut scratch = Vec::with_capacity(vec.capacity());
    scratch.extend(vec.iter().cloned());
    scratch.push(vec[0].clone());
    (scratch.capacity() - vec.capacity()) * size_of::<T>()
}

fn assert_resumed_refusal(
    site: &str,
    build: &dyn Fn() -> Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    headroom: usize,
    reserved: usize,
) {
    let (refused, control) = (build(), build());
    let _filler = fill_to(&refused.store, &refused.owner, headroom);
    let _control_filler = fill_to(&control.store, &control.owner, headroom);
    control
        .store
        .lock_state()
        .remove_prepared_build(control.request["cursor"].as_str().unwrap())
        .expect("parked control build");
    evict_unpinned(&control.store, &control.owner);
    refused.store.assert_conserved();
    let probes = build_probes(&refused.store);
    traced(&refused.store);
    assert!(
        !attempt(&refused),
        "{site}: headroom {headroom} admitted the site"
    );
    let trace = traced(&refused.store);
    assert_eq!(
        reserved_traces(&trace).iter().sum::<usize>(),
        reserved,
        "{site}: headroom {headroom} reserved past its refusal: {trace:?}"
    );
    assert!(
        !trace
            .iter()
            .any(|entry| matches!(entry, Trace::Allocated(_))),
        "{site}: headroom {headroom} allocated bookkeeping: {trace:?}"
    );
    refused.store.assert_conserved();
    assert_eq!(
        build_probes(&refused.store),
        probes,
        "{site}: the buffer was built before its admission at headroom {headroom}"
    );
    assert_eq!(
        (
            ledger(&refused.store),
            audited(&refused.store),
            bookkeeping(&refused.store),
            lease_table(&refused.store),
        ),
        (
            ledger(&control.store),
            audited(&control.store),
            bookkeeping(&control.store),
            lease_table(&control.store),
        ),
        "{site}: the refusal at headroom {headroom} did more than consume its parked build"
    );
    assert!(audited(&refused.store)[TOTAL] <= cap_for(&refused.owner));
    assert!(refused.store.lock_state().prepared_builds.is_empty());
}

fn parked_build_record_bytes(fixture: &Fixture) -> usize {
    let root = &fixture.pins[0];
    let state = fixture.store.lock_state();
    let builds: Vec<_> = state.prepared_builds.values().collect();
    let [build] = builds.as_slice() else {
        panic!("expected one parked build, found {}", builds.len());
    };
    assert!(
        state.prepared_facts.contains_key(&root.stamp.identity),
        "root facts are not cached"
    );
    size_of::<PreparedBuild>()
        + build.claimant.capacity()
        + value_bytes(&build.context)
        + value_bytes(&build.request)
        + value_bytes(&build.root_handle)
        + value_bytes(&build.classifier)
        + HashSet::from([root.stamp.identity.file()]).capacity() * size_of::<SourceIdentity>()
        + vec![(root.canonical_path.clone(), root.stamp)].capacity()
            * size_of::<(PathBuf, SourceStamp)>()
        + root.canonical_path.capacity()
}

#[test]
fn fresh_prepared_build_admits_its_record_before_constructing_it() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let site = "fresh prepared build record";
    let recorded = build_site(site, RECORDS);
    for background in [false, true] {
        let build = || direct_graph_fixture(&scenario, background);
        let probe = build();
        assert!(
            parked(&submit(&probe)),
            "{site}: the first step did not park"
        );
        let record = parked_build_record_bytes(&probe);
        assert_eq!(
            exact_headroom(&build, &recorded),
            REPLY_RESERVATION + record,
            "{site}: the record is not admitted at exactly its constructed bytes"
        );
        for headroom in [0, record - 1] {
            let fixture = build();
            let probes = build_probes(&fixture.store);
            assert_refused_at(
                site,
                &fixture,
                &|fixture: &Fixture| {
                    let admitted = recorded(fixture);
                    let trace = traced(&fixture.store);
                    assert_eq!(
                        reserved_traces(&trace),
                        [REPLY_RESERVATION],
                        "{site}: headroom {headroom} reserved build bytes: {trace:?}"
                    );
                    assert!(!trace
                        .iter()
                        .any(|entry| matches!(entry, Trace::Allocated(_))));
                    admitted
                },
                REPLY_RESERVATION + headroom + 1,
            );
            assert_eq!(
                build_probes(&fixture.store),
                probes,
                "{site}: the record was built before its admission at headroom {headroom}"
            );
            assert!(fixture.store.lock_state().prepared_builds.is_empty());
        }
        assert_fitted_at(site, &build(), &recorded, REPLY_RESERVATION + record);
    }
}

fn resumed_source_bytes(fixture: &Fixture, scenario: &Scenario) -> (usize, usize, usize) {
    let listed = std::fs::canonicalize(&scenario.sidechains[7]).unwrap();
    let directory = listed
        .parent()
        .unwrap()
        .join(listed.file_stem().unwrap())
        .join("subagents");
    let visited = std::fs::canonicalize(&scenario.sidechains[8]).unwrap();
    let identity = SourceStamp::of(&std::fs::metadata(&visited).unwrap())
        .identity
        .file();
    let state = fixture.store.lock_state();
    let (key, build) = state.prepared_builds.iter().next().expect("parked build");
    let mut seen = build.seen.clone();
    assert_eq!(seen.capacity(), build.seen.capacity());
    let before = seen.capacity();
    assert!(seen.insert(identity));
    (
        key.capacity(),
        value_bytes(&fixture.owner.clone())
            + directory.as_os_str().len()
            + grown_by_one(&build.sidechain_dirs),
        (seen.capacity() - before) * size_of::<SourceIdentity>()
            + grown_by_one(&build.stamps)
            + grown_by_one(&build.sources)
            + 3 * visited.capacity(),
    )
}

#[test]
fn resumed_prepared_build_admits_each_source_before_retaining_it() {
    let site = "resumed prepared build source";
    let padding = 192;
    let scenarios = [0, padding].map(|pad| {
        let mut scenario = Scenario::new(8, |index| line(&format!("thread-{index:04}")));
        let path = scenario.root.file(
            &format!("thread-0008{}.jsonl", "p".repeat(pad)),
            &line("thread-0008"),
        );
        scenario.sidechains.push(path);
        scenario
    });
    let sourced = build_site(site, SOURCES);
    for background in [false, true] {
        let build = |scenario: &Scenario| parked_build_fixture(scenario, background);
        let [short, long] = [&scenarios[0], &scenarios[1]]
            .map(|scenario| exact_headroom(&|| build(scenario), &sourced));
        assert_eq!(
            long - short,
            3 * padding,
            "{site}: a source's canonical path is not admitted once per retained copy"
        );
        let (released_key, before_site, at_site) =
            resumed_source_bytes(&build(&scenarios[0]), &scenarios[0]);
        assert_eq!(
            short + released_key,
            REPLY_RESERVATION + before_site + FILESYSTEM_PATH_BYTES + at_site,
            "{site}: the resumed step's admissions are not its measured buffers plus one path slot, net of the cursor key it releases"
        );
        for (headroom, reserved) in [
            (REPLY_RESERVATION, REPLY_RESERVATION),
            (
                short - 1,
                REPLY_RESERVATION + before_site + FILESYSTEM_PATH_BYTES,
            ),
        ] {
            assert_resumed_refusal(site, &|| build(&scenarios[0]), &sourced, headroom, reserved);
        }
        assert_fitted_at(site, &build(&scenarios[0]), &sourced, short);
    }
}

#[test]
fn stale_prepared_build_frees_its_buffers_before_releasing_their_reservation() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = parked_build_fixture(&scenario, background);
        let facts = {
            let mut state = fixture.store.lock_state();
            let cached = state
                .remove_prepared_facts(&fixture.pins[0].stamp.identity)
                .expect("cached root facts");
            Arc::downgrade(&cached.facts)
        };
        assert!(
            facts.upgrade().is_some(),
            "the parked build does not hold the root facts alone"
        );
        let releases = Arc::new(AtomicUsize::new(0));
        *fixture.store.release_hook.lock().unwrap() = Some(Arc::new({
            let (facts, releases) = (facts.clone(), Arc::clone(&releases));
            move |bytes: usize| {
                releases.fetch_add(1, Ordering::Relaxed);
                assert!(
                    facts.upgrade().is_none(),
                    "a {bytes}-byte reservation was released while its build was still live"
                );
            }
        }));
        let stale = fixture.store.request(
            &fixture.request,
            &restricted_context("resumed-build", background, &scenario.root.directory, 0),
            &Cancellation::default(),
        );
        *fixture.store.release_hook.lock().unwrap() = None;
        assert_eq!(stale["status"].as_str(), Some("stale_cursor"), "{stale:?}");
        assert_eq!(
            releases.load(Ordering::Relaxed),
            2,
            "the stale resume did not release its step and reply reservations"
        );
        assert!(facts.upgrade().is_none());
        assert!(fixture.store.lock_state().prepared_builds.is_empty());
        fixture.store.assert_conserved();
    }
}

#[test]
fn retained_root_facts_stay_counted_across_a_cache_replacement() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let classifier = json!({"id":"native","version":"1"});
    for background in [false, true] {
        let store = prepared_store();
        let owner = context_for("facts-owner", background);
        let other = restricted_context("facts-owner", background, &scenario.root.directory, 0);
        let (_, root) = acquired(&store, &scenario.root.path, &owner);
        let (retained, reservation) = store
            .prepared_root_facts(
                &root,
                &classifier,
                &owner,
                &work_bounds(),
                &Cancellation::default(),
            )
            .unwrap();
        let bytes = NativeStore::audit_facts_bytes(retained.facts());
        assert_eq!(store.lock_state().ledger.shared.facts(), bytes);
        let (replacement, replacement_reservation) = store
            .prepared_root_facts(
                &root,
                &classifier,
                &other,
                &work_bounds(),
                &Cancellation::default(),
            )
            .unwrap();
        {
            let state = store.lock_state();
            assert!(Arc::ptr_eq(
                &state.prepared_facts[&root.stamp.identity].facts,
                replacement.facts()
            ));
            assert!(!Arc::ptr_eq(retained.facts(), replacement.facts()));
            assert_eq!(
                state.ledger.shared.facts(),
                bytes + NativeStore::audit_facts_bytes(replacement.facts()),
                "the replaced root facts lost their in-flight owner"
            );
        }
        store.assert_conserved();
        drop(retained);
        assert_eq!(
            store.lock_state().ledger.shared.facts(),
            NativeStore::audit_facts_bytes(replacement.facts())
        );
        drop(replacement_reservation);
        drop(reservation);
        store.assert_conserved();
    }
}

#[test]
fn resumed_prepared_build_keeps_its_root_facts_counted_after_a_cache_replacement() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = parked_build_fixture(&scenario, background);
        let other = restricted_context("other", background, &scenario.root.directory, 0);
        let (root, _pin) = acquired(&fixture.store, &scenario.root.path, &other);
        let replaced = drive(
            &fixture.store,
            fixture.store.request(
                &prepare_request(&root, &[], &[], &scenario.direct()),
                &other,
                &Cancellation::default(),
            ),
            &other,
        );
        assert_eq!(replaced["status"].as_str(), Some("ok"), "{replaced:?}");
        let (parked, cached) = {
            let state = fixture.store.lock_state();
            let build = state.prepared_builds.values().next().expect("parked build");
            (
                Arc::clone(&build.root_facts),
                Arc::clone(&state.prepared_facts[&fixture.pins[0].stamp.identity].facts),
            )
        };
        assert!(
            !Arc::ptr_eq(&parked, &cached),
            "the other authority did not replace the cached root facts"
        );
        let counted = fixture.store.lock_state().ledger.shared.facts();
        assert_eq!(
            counted,
            NativeStore::audit_facts_bytes(&parked) + NativeStore::audit_facts_bytes(&cached)
        );
        let (resumed, (ledgered, audit, extracted)) = returned_facts_barrier(&fixture, || {
            (
                ledger(&fixture.store)[TOTAL],
                audited(&fixture.store)[TOTAL],
                fixture.store.lock_state().ledger.shared.facts(),
            )
        });
        assert_eq!(
            audit, ledgered,
            "the audit lost the extracted build's root facts"
        );
        assert_eq!(
            extracted, counted,
            "the extracted build dropped its root facts' owner"
        );
        let resumed = drive(&fixture.store, resumed, &fixture.owner);
        assert_eq!(resumed["status"].as_str(), Some("ok"), "{resumed:?}");
        assert_eq!(
            fixture.store.lock_state().ledger.shared.facts(),
            counted,
            "the resumed build dropped or double counted its root facts"
        );
        fixture.store.assert_conserved();
    }
}

fn cached_root_facts_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("cached-root-facts", background);
    let (root, snapshot) = acquired(&store, &source.path, &owner);
    let request = prepare_request(&root, &[], &[], &[]);
    let primed = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(primed["status"].as_str(), Some("ok"), "{primed:?}");
    assert!(
        store
            .lock_state()
            .prepared_facts
            .contains_key(&snapshot.stamp.identity),
        "the root facts were not cached"
    );
    Fixture {
        store,
        owner,
        request,
        pins: vec![snapshot],
    }
}

fn cached_source_facts_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("cached-source-facts", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let (_, sidechain) = acquired(&store, &scenario.sidechains[0], &owner);
    let graph = prepared_graph(&store, &root, &scenario.direct(), &owner);
    let request = graph_query(&graph, missing_tool(), json!([]));
    let primed = drive(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(primed["status"].as_str(), Some("ok"), "{primed:?}");
    assert!(
        store
            .lock_state()
            .prepared_facts
            .contains_key(&sidechain.stamp.identity),
        "the sidechain facts were not cached"
    );
    Fixture {
        store,
        owner,
        request,
        pins: vec![root_snapshot, sidechain],
    }
}

fn cache_replacement_barrier(
    site: &str,
    fixture: &Fixture,
    identity: SourceIdentity,
    replace: impl FnOnce() -> Value,
) -> Replacement {
    let cached = || Arc::clone(&fixture.store.lock_state().prepared_facts[&identity].facts);
    let (response, (held, before, replaced, fresh, counted, ledgered, audit)) =
        returned_facts_barrier(fixture, || {
            let held = cached();
            let before = fixture.store.lock_state().ledger.shared.facts();
            let replaced = replace();
            let fresh = cached();
            let counted = fixture.store.lock_state().ledger.shared.facts();
            let ledgered = ledger(&fixture.store)[TOTAL];
            let audit = audited(&fixture.store)[TOTAL];
            (held, before, replaced, fresh, counted, ledgered, audit)
        });
    assert_eq!(
        replaced["status"].as_str(),
        Some("ok"),
        "{site}: {replaced:?}"
    );
    assert!(
        !Arc::ptr_eq(&held, &fresh),
        "{site}: the other authority did not replace the cached facts"
    );
    assert_eq!(
        counted,
        before + NativeStore::audit_facts_bytes(&fresh),
        "{site}: the replaced facts lost their in-flight owner"
    );
    assert_eq!(audit, ledgered, "{site}: the audit lost the returned facts");
    Replacement {
        response,
        held,
        fresh,
        counted,
    }
}

#[test]
fn cached_root_facts_stay_counted_across_a_cache_replacement() {
    let source = LedgerSource::new(&lines(0..4));
    let site = "prepare_graph cache hit replacement barrier";
    for background in [false, true] {
        let fixture = cached_root_facts_fixture(&source, background);
        let other = restricted_context("cached-root-facts", background, &source.directory, 0);
        let replacement =
            cache_replacement_barrier(site, &fixture, fixture.pins[0].stamp.identity, || {
                let (root, _pin) = acquired(&fixture.store, &source.path, &other);
                drive(
                    &fixture.store,
                    fixture.store.request(
                        &prepare_request(&root, &[], &[], &[]),
                        &other,
                        &Cancellation::default(),
                    ),
                    &other,
                )
            });
        assert_eq!(
            replacement.response["status"].as_str(),
            Some("ok"),
            "{site}: {:?}",
            replacement.response
        );
        assert_eq!(
            replacement.response["data"]["kind"].as_str(),
            Some("prepared_graph"),
            "{site}: {:?}",
            replacement.response
        );
        assert_eq!(
            fixture.store.lock_state().ledger.shared.facts(),
            NativeStore::audit_facts_bytes(&replacement.held)
                + NativeStore::audit_facts_bytes(&replacement.fresh),
            "{site}: the published graph does not own the facts it was built from"
        );
        fixture.store.assert_conserved();
    }
}

#[test]
fn cached_source_facts_stay_counted_across_a_cache_replacement() {
    let scenario = Scenario::new(1, |index| line(&format!("thread-{index:04}")));
    let site = "prepared_query_page cache hit replacement barrier";
    for background in [false, true] {
        let fixture = cached_source_facts_fixture(&scenario, background);
        let other = restricted_context(
            "cached-source-facts",
            background,
            &scenario.root.directory,
            0,
        );
        let (root, _pin) = acquired(&fixture.store, &scenario.root.path, &other);
        let query = graph_query(
            &prepared_graph(&fixture.store, &root, &scenario.direct(), &other),
            missing_tool(),
            json!([]),
        );
        let replacement =
            cache_replacement_barrier(site, &fixture, fixture.pins[1].stamp.identity, || {
                drive(
                    &fixture.store,
                    fixture
                        .store
                        .request(&query, &other, &Cancellation::default()),
                    &other,
                )
            });
        assert_eq!(
            replacement.response["status"].as_str(),
            Some("ok"),
            "{site}: {:?}",
            replacement.response
        );
        assert_eq!(
            replacement.response["data"],
            json!({"kind":"scalar","value":false}),
            "{site}: {:?}",
            replacement.response
        );
        assert_eq!(
            fixture.store.lock_state().ledger.shared.facts()
                + NativeStore::audit_facts_bytes(&replacement.held),
            replacement.counted,
            "{site}: the consumed facts outlived their owner in the ledger"
        );
        fixture.store.assert_conserved();
    }
}

#[test]
fn realpath_resolves_within_the_filesystem_path_slot() {
    let source = LedgerSource::new(&line("root"));
    let resolved = realpath(&source.path).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&source.path).unwrap());
    assert_eq!(resolved.capacity(), resolved.as_os_str().len());
    assert!(resolved.as_os_str().len() < libc::PATH_MAX as usize);
    assert_eq!(
        realpath(Path::new(".")).unwrap(),
        std::fs::canonicalize(".").unwrap()
    );
    assert_eq!(
        realpath(&source.directory.join("missing"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        realpath(Path::new(&"x".repeat(libc::PATH_MAX as usize)))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENAMETOOLONG)
    );
    assert_eq!(
        realpath(Path::new("a\0b")).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "moves the process working directory below PATH_MAX"]
fn realpath_refuses_a_working_directory_longer_than_path_max() {
    struct WorkingDirectory(PathBuf);

    impl Drop for WorkingDirectory {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    let _restored = WorkingDirectory(std::env::current_dir().unwrap());
    let source = LedgerSource::new(&line("root"));
    std::env::set_current_dir(&source.directory).unwrap();
    let component = "d".repeat(200);
    for _ in 0..=libc::PATH_MAX as usize / component.len() {
        std::fs::create_dir(&component).unwrap();
        std::env::set_current_dir(&component).unwrap();
    }
    assert_eq!(
        realpath(Path::new(".")).unwrap_err().raw_os_error(),
        Some(libc::ENAMETOOLONG)
    );
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
        arc_mirror::<EntryChunk>()
            + arc_mirror::<ChunkRows>()
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
            arc_slice_mirror::<PreparedSourceRef>(membership.members.len())
                + membership
                    .members
                    .iter()
                    .map(|member| member.path.capacity())
                    .sum::<usize>()
                + arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(
                    membership.sidechain_dirs.len(),
                )
                + membership
                    .sidechain_dirs
                    .iter()
                    .map(|(path, _)| path.capacity())
                    .sum::<usize>(),
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

fn delivered(store: &NativeStore) -> usize {
    let state = store.lock_state();
    state
        .deliveries
        .audit_with(NativeStore::audit_delivery_bytes)
        + state.deliveries_expiry.index_bytes()
}

fn refused_at_site(
    site: &str,
    build: &dyn Fn() -> Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    offset: usize,
    exact: usize,
) {
    for headroom in [offset, exact - 1] {
        assert_refused_at(site, &build(), attempt, headroom + 1);
    }
    assert_fitted_at(site, &build(), attempt, exact);
}

fn location_park_fixture(source: &LedgerSource, background: bool) -> Fixture {
    Fixture {
        store: cursor_store(),
        owner: context_for("locate-park", background),
        request: json!({"schema":SCHEMA,"id":"ledger-locate","operation":"locate","session_ids":["absent"],"roots":[source.directory.to_string_lossy().as_ref()],"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        pins: Vec::new(),
    }
}

fn location_parked(fixture: &Fixture) -> bool {
    match fixture.store.locate(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &mut [0u64; 18],
    ) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_some() && reason.is_some(),
                "the location page completed: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("location park failed outside admission: {error:?}"),
    }
}

fn open_directory_reservation(cursor: &LocateCursor, root: &Path) -> usize {
    cursor.seen_directories.capacity() * size_of::<SourceIdentity>()
        + cursor.directories.capacity() * size_of::<OpenDirectory>()
        + arc_mirror::<(*mut libc::DIR, PathBuf)>()
        + root.as_os_str().len()
}

fn location_step_peak(cursor: &LocateCursor, root: &Path) -> usize {
    locate_record_bytes(cursor) + LOCATE_PATH_SLOTS + open_directory_reservation(cursor, root)
}

#[test]
fn location_cursor_park_admits_its_stored_key_exactly() {
    let source = LedgerSource::new(&line("locate"));
    for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
        source.file(name, &line(name));
    }
    for background in [false, true] {
        let build = || location_park_fixture(&source, background);
        let exact = exact_headroom(&build, &location_parked);
        refused_at_site("location park", &build, &location_parked, 0, exact);
        let fitted = build();
        let capacity = fitted.store.lock_state().locates.capacity_bytes();
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let released = Arc::new(Mutex::new(Vec::new()));
        *fitted.store.release_hook.lock().unwrap() = Some(Arc::new({
            let released = Arc::clone(&released);
            move |bytes: usize| released.lock().unwrap().push(bytes)
        }));
        assert!(location_parked(&fitted));
        *fitted.store.release_hook.lock().unwrap() = None;
        let trace = traced(&fitted.store);
        assert_admitted_before_allocating("location park", &trace);
        fitted.store.assert_conserved();
        let (token, pledged, parked) = {
            let state = fitted.store.lock_state();
            let (token, cursor) = state.locates.iter().next().expect("parked location cursor");
            assert_eq!(token.capacity(), 64);
            assert!(
                reserved_traces(&trace)
                    .contains(&open_directory_reservation(cursor, &source.directory)),
                "the location step did not reserve its directory tables, root copy, and open-directory handle in one extension: {trace:?}"
            );
            let parked = NativeStore::audit_locate_bytes(token, cursor)
                + state.locates.capacity_bytes()
                - capacity
                + state.locates.pledged(token);
            assert_eq!(
                exact,
                location_step_peak(cursor, &source.directory).max(parked),
                "the location step's admission is not its peak reservation or its stored key, cursor, table growth, and pledge"
            );
            (token.clone(), state.locates.pledged(token), parked)
        };
        assert_eq!(
            *released.lock().unwrap(),
            vec![exact - parked],
            "the location park did not draw its stored key, cursor, table growth, and pledge from the step's reservation"
        );
        let cap = cap_for(&fitted.owner);
        assert_eq!(
            ledger(&fitted.store)[TOTAL],
            cap - exact + parked,
            "the fitted location step retained more than its parked record"
        );
        assert_eq!(audited(&fitted.store)[TOTAL], cap - exact + parked);
        let before = delivered(&fitted.store);
        traced(&fitted.store);
        fitted
            .store
            .track_delivery(
                &json!({"id":"ledger-delivery","status":"incomplete","cursor":token}),
                &fitted.owner,
                false,
            )
            .unwrap();
        assert!(
            traced(&fitted.store).is_empty(),
            "converting the location pledge admitted or allocated"
        );
        assert_eq!(
            delivered(&fitted.store) - before,
            pledged,
            "the location pledge differs from its delivery record"
        );
        let refused = build();
        {
            let _filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(!location_parked(&refused));
        }
        assert!(
            refused.store.lock_state().locates.is_empty(),
            "a refused location cursor was parked"
        );
        assert!(
            location_parked(&refused),
            "the store was unusable after a refused location park"
        );
        refused.store.assert_conserved();
    }
}

fn load_slot_fixture(source: &LedgerSource, name: &str, background: bool) -> Fixture {
    let parent = source.directory.join(name);
    std::fs::create_dir_all(&parent).unwrap();
    let path = parent.join("s.jsonl");
    std::fs::write(&path, lines(0..2)).unwrap();
    let store = fast_store();
    let owner = context_for("load-slot", background);
    let (_, snapshot) = acquired(&store, &path, &owner);
    store.lock_state().retain_loads(|_, _| false);
    Fixture {
        store,
        owner,
        request: acquire(&path),
        pins: vec![snapshot],
    }
}

fn cached_acquire(fixture: &Fixture) -> bool {
    match fixture.store.acquire(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &mut [0u64; 18],
    ) {
        Ok((data, cursor, reason)) => {
            assert!(cursor.is_none() && reason.is_none(), "{data:?}");
            assert_eq!(data["kind"].as_str(), Some("acquired"), "{data:?}");
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("cached acquire failed outside admission: {error:?}"),
    }
}

fn empty_index_walk(slot: &LoadSlot) -> usize {
    slot.work
        .lock()
        .unwrap()
        .activity
        .audited_heap_allocations()
        .iter()
        .map(|(_, bytes)| bytes)
        .sum()
}

fn nested_name(byte: &str) -> String {
    format!("{0}/{0}", byte.repeat(150))
}

#[test]
fn load_slot_admits_and_charges_its_record_once() {
    let source = LedgerSource::new(&line("anchor"));
    let site = "load slot";
    for background in [false, true] {
        let [small, large] = ["d".to_owned(), nested_name("d")].map(|name| {
            exact_headroom(
                &|| load_slot_fixture(&source, &name, background),
                &cached_acquire,
            )
        });
        assert_eq!(
            large - small,
            300,
            "{site}: the slot path is not charged once by its bytes"
        );
        let name = nested_name("d");
        let build = || load_slot_fixture(&source, &name, background);
        assert_refused_at(site, &build(), &cached_acquire, 1);
        {
            let released = settled(&build().store);
            let refused = build();
            let cap = cap_for(&refused.owner);
            let filler = fill_to(&refused.store, &refused.owner, large - 1);
            assert!(
                !cached_acquire(&refused),
                "{site}: one byte over the exact fit was admitted"
            );
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap);
            drop(filler);
            assert_eq!(
                settled(&refused.store),
                released,
                "{site}: the refused acquire left its slot behind"
            );
            assert!(refused.store.lock_state().loads.is_empty());
        }
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let (pending, reserved) = {
            let state = fitted.store.lock_state();
            assert!(state.loads.is_empty());
            (state.ledger.pending, state.loads.reserved_bytes())
        };
        let _filler = fill_to(&fitted.store, &fitted.owner, large);
        traced(&fitted.store);
        assert!(cached_acquire(&fitted), "{site}: the exact fit was refused");
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        let slot = fitted
            .store
            .lock_state()
            .loads
            .values()
            .next()
            .cloned()
            .expect("cached load slot");
        let empty = empty_index_walk(&slot);
        assert!(empty > 0);
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            empty,
            "{site}: the cached slot does not carry its empty index"
        );
        let record = NativeStore::audit_load_record_bytes(&slot);
        let state = fitted.store.lock_state();
        let growth = state.loads.reserved_bytes() - reserved;
        assert_eq!(
            state.ledger.pending - pending,
            record + empty,
            "{site}: the pending gauge misses the slot record or its empty index"
        );
        let admitted = trace
            .iter()
            .position(|entry| matches!(entry, Trace::Admitted(_)))
            .unwrap_or_else(|| panic!("{site}: nothing was admitted: {trace:?}"));
        assert_eq!(
            trace[admitted],
            Trace::Admitted(growth + record + empty),
            "{site}: the slot admission is not its table growth, record, and empty index"
        );
        assert!(
            trace
                .iter()
                .enumerate()
                .all(|(at, entry)| !matches!(entry, Trace::Allocated(_)) || at > admitted),
            "{site}: retained bookkeeping grew before the slot admission: {trace:?}"
        );
    }
}

fn cold_load_fixture(source: &LedgerSource, background: bool) -> Fixture {
    Fixture {
        store: slow_store(),
        owner: context_for("cold-load", background),
        request: acquire(&source.path),
        pins: Vec::new(),
    }
}

fn cold_acquire(fixture: &Fixture) -> bool {
    match fixture.store.acquire(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &mut [0u64; 18],
    ) {
        Ok((data, cursor, reason)) => {
            assert!(cursor.is_some() && reason.is_some(), "{data:?}");
            assert_eq!(data["kind"].as_str(), Some("loading"), "{data:?}");
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("cold acquire failed outside admission: {error:?}"),
    }
}

fn cold_load_walk(slot: &LoadSlot) -> usize {
    let load = slot.work.lock().unwrap();
    let Load {
        file: _,
        offset: _,
        pending,
        pending_start: _,
        provider: _,
        chunks,
        codex_raw,
        codex_append,
        count: _,
        activity,
        indexed: _,
        decoded: _,
        session_id,
        origin_fence,
        origin_complete: _,
        seal_fence,
        sealed: _,
        prefix_fence,
        previous,
        previous_index_compatible: _,
        prefix_checked: _,
        window_scanned: _,
        window_start: _,
        fence,
        committed: _,
        provisional: _,
        result,
        failure: _,
    } = &*load;
    assert!(
        previous.is_none() && result.is_none(),
        "the load is not cold"
    );
    assert!(codex_raw.is_none() && codex_append.is_none());
    pending.capacity()
        + origin_fence.capacity()
        + seal_fence.capacity()
        + prefix_fence.capacity()
        + fence.capacity()
        + session_id.as_ref().map_or(0, String::capacity)
        + chunks.capacity() * size_of::<Arc<EntryChunk>>()
        + chunks
            .iter()
            .map(|chunk| NativeStore::audit_chunk_bytes(chunk))
            .sum::<usize>()
        + activity
            .audited_heap_allocations()
            .iter()
            .map(|(_, bytes)| bytes)
            .sum::<usize>()
}

#[test]
fn incomplete_loads_charge_their_inline_index_once() {
    let source = LedgerSource::new(&lines(0..4));
    let site = "cold load";
    for background in [false, true] {
        let build = || cold_load_fixture(&source, background);
        let exact = exact_headroom(&build, &cold_acquire);
        assert_refused_at(site, &build(), &cold_acquire, 1);
        {
            let released = settled_charges(&build().store);
            let refused = build();
            let cap = cap_for(&refused.owner);
            let filler = fill_to(&refused.store, &refused.owner, exact - 1);
            assert!(
                !cold_acquire(&refused),
                "{site}: one byte over the exact fit was admitted"
            );
            refused.store.assert_conserved();
            assert!(audited(&refused.store)[TOTAL] <= cap);
            drop(filler);
            assert_eq!(
                settled_charges(&refused.store),
                released,
                "{site}: the refused acquire left its slot behind"
            );
            assert!(refused.store.lock_state().loads.is_empty());
        }
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        assert!(cold_acquire(&fitted), "{site}: the exact fit was refused");
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        let slot = fitted
            .store
            .lock_state()
            .loads
            .values()
            .next()
            .cloned()
            .expect("parked cold load");
        let walk = cold_load_walk(&slot);
        assert!(walk > empty_index_walk(&slot));
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            walk,
            "{site}: the slot charge is not its heap walked once"
        );
        assert_eq!(
            fitted.store.lock_state().ledger.pending,
            NativeStore::audit_load_record_bytes(&slot) + walk,
            "{site}: the pending gauge is not the slot record plus its heap"
        );
    }
}

fn array_tool_line(id: &str, zeros: usize) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":format!("{id}-call"),"name":"Bash","input":{"n":vec![0u8; zeros]}}]}})
    )
}

fn advance_parked(fixture: &Fixture) -> bool {
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
            assert!(cursor.is_some() && reason.is_some(), "{data:?}");
            assert_eq!(data["kind"].as_str(), Some("loading"), "{data:?}");
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("advance failed outside admission: {error:?}"),
    }
}

fn admitted_traces(store: &NativeStore) -> Vec<usize> {
    traced(store)
        .into_iter()
        .filter_map(|trace| match trace {
            Trace::Admitted(bytes) => Some(bytes),
            Trace::Allocated(_) | Trace::Reserved(_) => None,
        })
        .collect()
}

fn step_reservation(read_step: usize, pending: usize) -> (usize, usize) {
    let decode_bound = 4 * (64 * 1024usize).min(pending + read_step);
    (read_step + decode_bound, decode_bound)
}

#[test]
fn metered_decode_steps_stop_at_their_decode_bound() {
    let wide: String = (0..5)
        .map(|index| array_tool_line(&format!("wide-{index}"), 4096))
        .collect();
    let source = LedgerSource::new(&format!("{}{wide}", line("anchor")));
    let store = fast_store();
    let owner = context_for("metered", false);
    let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    assert!(parked(&response), "{response:?}");
    let slot = store
        .lock_state()
        .loads
        .values()
        .next()
        .cloned()
        .expect("parked load");
    let (reservation, _) = step_reservation(64 * 1024, 0);
    let mut stopped = None;
    let mut steps = 0;
    while response["status"].as_str() == Some("incomplete") {
        store.assert_conserved();
        let before = slot.accounted.load(Ordering::Acquire);
        traced(&store);
        response = resume(&store, &response, &owner);
        let admitted = admitted_traces(&store);
        if response["status"].as_str() == Some("ok") {
            break;
        }
        assert!(parked(&response), "{response:?}");
        let reserved = admitted
            .iter()
            .position(|&bytes| bytes == reservation)
            .unwrap_or_else(|| panic!("step {steps} never reserved {reservation}: {admitted:?}"));
        let step = &admitted[reserved..];
        assert_eq!(
            step.last(),
            Some(&0),
            "step {steps}: the exact charge exceeded its admitted reservation: {admitted:?}"
        );
        let after = slot.accounted.load(Ordering::Acquire);
        assert!(
            after.saturating_sub(before) <= step.iter().sum::<usize>(),
            "step {steps}: retained {} past the {} admitted",
            after.saturating_sub(before),
            step.iter().sum::<usize>()
        );
        {
            let load = slot.work.lock().unwrap();
            if !load.decoded && load.count > 0 && load.pending.contains(&b'\n') {
                stopped = Some(load.count);
            }
        }
        steps += 1;
    }
    assert!(
        matches!(stopped, Some(count) if count < 6),
        "the decode never stopped short of its bound: {stopped:?}"
    );
    let snapshot = store.pin(&ok_handle(&response), &owner).unwrap();
    assert_eq!(snapshot.event_count, 6);
    assert!(snapshot.chunks.len() >= 2, "{}", snapshot.chunks.len());
    let whole = crate::parse::parse_bytes(&std::fs::read(&source.path).unwrap(), |_| true).unwrap();
    assert_eq!(
        snapshot
            .entries()
            .iter()
            .map(|entry| entry.meta().map(|meta| meta.uuid.clone()))
            .collect::<Vec<_>>(),
        whole
            .iter()
            .map(|entry| entry.meta().map(|meta| meta.uuid.clone()))
            .collect::<Vec<_>>()
    );
    store.assert_conserved();
}

fn wide_line_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = store_with(64 * 1024, 1, &[]);
    let owner = context_for("wide-line", background);
    let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    for _ in 0..2 {
        assert!(parked(&response), "{response:?}");
        response = resume(&store, &response, &owner);
    }
    assert!(parked(&response), "{response:?}");
    {
        let slot = store
            .lock_state()
            .loads
            .values()
            .next()
            .cloned()
            .expect("parked load");
        let load = slot.work.lock().unwrap();
        assert!(
            !load.decoded && load.count == 1 && load.indexed == 1 && load.pending.contains(&b'\n'),
            "the load did not park before the wide line"
        );
    }
    Fixture {
        store,
        owner,
        request: response,
        pins: Vec::new(),
    }
}

#[test]
fn wide_decode_lines_are_admitted_exactly_before_they_are_retained() {
    let source = LedgerSource::new(&format!(
        "{}{}{}",
        line("anchor"),
        array_tool_line("wide", 20480),
        line("trailer")
    ));
    let site = "wide decode line";
    for background in [false, true] {
        let build = || wide_line_fixture(&source, background);
        let exact = exact_headroom(&build, &advance_parked);
        let probe = build();
        let (reservation, decode_bound, chunks_growth, fence_growth) = {
            let slot = parked_slot(&probe);
            let load = slot.work.lock().unwrap();
            let (reservation, decode_bound) = step_reservation(64 * 1024, load.pending.len());
            (
                reservation,
                decode_bound,
                grown_by_one(&load.chunks),
                64usize.saturating_sub(load.fence.capacity()),
            )
        };
        let refused = build();
        let slot = parked_slot(&refused);
        let before = slot.accounted.load(Ordering::Acquire);
        assert_refused_at(site, &refused, &advance_parked, exact);
        {
            let load = slot.work.lock().unwrap();
            assert_eq!(load.count, 1, "{site}: the refusal retained the wide line");
            assert_eq!(load.chunks.len(), 1);
            assert!(load.pending.contains(&b'\n'));
            assert!(
                load.failure.is_none(),
                "{site}: the refusal poisoned the load"
            );
        }
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            before,
            "{site}: the refusal moved the load charge"
        );
        let fitted = build();
        let slot = parked_slot(&fitted);
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        assert!(advance_parked(&fitted), "{site}: the exact fit was refused");
        let admitted = admitted_traces(&fitted.store);
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        let wide = {
            let load = slot.work.lock().unwrap();
            assert_eq!(
                load.count, 2,
                "{site}: the exact fit did not retain the wide line"
            );
            assert!(load.pending.contains(&b'\n'));
            NativeStore::audit_chunk_bytes(&load.chunks[1])
        };
        let extension = wide + chunks_growth + fence_growth - decode_bound;
        assert_eq!(
            admitted,
            vec![reservation, extension, 0],
            "{site}: the wide line was not admitted exactly before its retention"
        );
        assert_eq!(exact, reservation + extension);
        let walk = cold_load_walk(&slot);
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            walk,
            "{site}: the slot charge is not its heap walked once"
        );
        let pending = fitted.store.lock_state().ledger.pending;
        assert_eq!(
            pending,
            NativeStore::audit_load_record_bytes(&slot) + walk,
            "{site}: the pending gauge is not the slot record plus its heap"
        );
    }
}

fn appended_index_fixture(source: &LedgerSource, background: bool) -> Fixture {
    std::fs::write(&source.path, lines(0..SEEDED_EVENTS)).unwrap();
    let store = prepared_store();
    let owner = context_for("appended", background);
    let (_, previous) = acquired(&store, &source.path, &owner);
    assert_eq!(previous.event_count, SEEDED_EVENTS);
    source.append(&lines(SEEDED_EVENTS..SEEDED_EVENTS + 2 * SEEDED_STEP));
    let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
    loop {
        assert!(parked(&response), "{response:?}");
        let ready = {
            let slot = store
                .lock_state()
                .loads
                .values()
                .next()
                .cloned()
                .expect("parked load");
            let load = slot.work.lock().unwrap();
            load.previous.is_some() && load.decoded && load.indexed < load.count
        };
        if ready {
            break;
        }
        response = resume(&store, &response, &owner);
    }
    Fixture {
        store,
        owner,
        request: response,
        pins: vec![previous],
    }
}

fn appended_index_walk(slot: &LoadSlot) -> usize {
    let load = slot.work.lock().unwrap();
    let previous = load
        .previous
        .as_ref()
        .expect("the load extends a published snapshot");
    assert!(load.result.is_none() && load.codex_raw.is_none() && load.codex_append.is_none());
    let shared: HashSet<usize> = previous
        .chunks
        .iter()
        .map(|chunk| Arc::as_ptr(&chunk.entries) as usize)
        .collect();
    let prior: HashSet<usize> = previous
        .activity
        .audited_allocations(true)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    load.pending.capacity()
        + load.origin_fence.capacity()
        + load.seal_fence.capacity()
        + load.prefix_fence.capacity()
        + load.fence.capacity()
        + load.session_id.as_ref().map_or(0, String::capacity)
        + load.chunks.capacity() * size_of::<Arc<EntryChunk>>()
        + load
            .chunks
            .iter()
            .filter(|chunk| !shared.contains(&(Arc::as_ptr(&chunk.entries) as usize)))
            .map(|chunk| NativeStore::audit_chunk_bytes(chunk))
            .sum::<usize>()
        + load
            .activity
            .audited_heap_allocations()
            .into_iter()
            .filter(|(id, _)| !prior.contains(id))
            .map(|(_, bytes)| bytes)
            .sum::<usize>()
}

#[test]
fn appended_index_steps_admit_their_copy_on_write_before_appending() {
    let source = LedgerSource::new("");
    let site = "appended index step";
    for background in [false, true] {
        let build = || appended_index_fixture(&source, background);
        let exact = exact_headroom(&build, &advance_parked);
        let probe = build();
        let (reservation, extension, heuristic) = {
            let slot = parked_slot(&probe);
            let load = slot.work.lock().unwrap();
            let (reservation, _) = step_reservation(4096, load.pending.len());
            let (bytes, calls, results) = load
                .chunks
                .iter()
                .filter(|chunk| chunk.start >= load.indexed)
                .flat_map(|chunk| chunk.entries.iter().zip(chunk.entry_charges.iter()))
                .fold(
                    (0usize, 0usize, 0usize),
                    |(bytes, calls, results), (entry, charge)| {
                        (
                            bytes + charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
                            calls + entry.tool_uses().count(),
                            results + entry.tool_results().count(),
                        )
                    },
                );
            let appended = load.count - load.indexed;
            assert_eq!(appended, 2 * SEEDED_STEP);
            let heuristic = 2 * bytes + appended * size_of::<&Entry>();
            let previous = load
                .previous
                .as_ref()
                .expect("the load extends a published snapshot");
            let indexed: Vec<&Entry> = (0..load.indexed)
                .map(|position| previous.entry(position))
                .collect();
            (
                reservation,
                heuristic
                    + append_reservation_mirror(
                        &load.activity,
                        &previous.activity,
                        &indexed,
                        appended,
                        calls,
                        results,
                    ),
                heuristic,
            )
        };
        assert_eq!(
            exact,
            reservation + extension,
            "{site}: the index step admission is not the step reservation plus its extension"
        );
        let refused = build();
        let slot = parked_slot(&refused);
        let before = slot.accounted.load(Ordering::Acquire);
        assert_refused_at(site, &refused, &advance_parked, exact);
        {
            let load = slot.work.lock().unwrap();
            assert_eq!(load.indexed, SEEDED_EVENTS, "{site}: the refusal appended");
            assert_eq!(load.activity.entry_count(), SEEDED_EVENTS);
            assert!(
                load.failure.is_none(),
                "{site}: the refusal poisoned the load"
            );
        }
        assert_eq!(slot.accounted.load(Ordering::Acquire), before);
        assert_eq!(appended_index_walk(&slot), before);
        let fitted = build();
        let slot = parked_slot(&fitted);
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        assert!(advance_parked(&fitted), "{site}: the exact fit was refused");
        let admitted = admitted_traces(&fitted.store);
        fitted.store.assert_conserved();
        assert!(audited(&fitted.store)[TOTAL] <= cap);
        assert_eq!(
            admitted,
            vec![reservation, extension, 0],
            "{site}: the copy-on-write was not admitted exactly before the append"
        );
        let after = slot.accounted.load(Ordering::Acquire);
        assert_eq!(
            slot.work.lock().unwrap().indexed,
            SEEDED_EVENTS + 2 * SEEDED_STEP
        );
        assert_eq!(
            after,
            appended_index_walk(&slot),
            "{site}: the slot charge is not its owned heap walked once"
        );
        assert!(
            after - before > heuristic,
            "{site}: the copy-on-write growth {} fits the input heuristic {heuristic}",
            after - before
        );
        let pending = fitted.store.lock_state().ledger.pending;
        assert_eq!(
            pending,
            NativeStore::audit_load_record_bytes(&slot) + after,
            "{site}: the pending gauge is not the slot record plus its heap"
        );
    }
}

struct Backstop {
    store: &'static NativeStore,
    source: LedgerSource,
    parked_owner: Value,
    parked_response: Value,
    slot: Arc<LoadSlot>,
    held: Arc<Mutex<Option<ProjectionReservation<'static>>>>,
}

fn refused_backstop(joined: bool) -> Backstop {
    let source = LedgerSource::new(&lines(0..3));
    let store: &'static NativeStore = Box::leak(Box::new(prepared_store()));
    let parked_owner = context_for("backstop-parked", true);
    let parked_response = store.request(
        &acquire(&source.path),
        &parked_owner,
        &Cancellation::default(),
    );
    assert!(parked(&parked_response), "{parked_response:?}");
    let slot = store
        .lock_state()
        .loads
        .values()
        .next()
        .cloned()
        .expect("parked load");
    store
        .lock_state()
        .insert_prepared_load(slot.stamp.identity, Arc::clone(&slot), now_ms());
    store.assert_conserved();
    let foreground = context_for("filler", false);
    let held: Arc<Mutex<Option<ProjectionReservation<'static>>>> = Arc::new(Mutex::new(None));
    *store.read_hook.lock().unwrap() = Some(Arc::new({
        let held = Arc::clone(&held);
        let foreground = foreground.clone();
        move || {
            let headroom = cap_of(store, &foreground) - ledger(store)[TOTAL];
            *held.lock().unwrap() = Some(store.reserve_projection(&foreground, headroom).unwrap());
        }
    }));
    let refused = if joined {
        store.request(
            &acquire(&source.path),
            &context_for("backstop-joined", true),
            &Cancellation::default(),
        )
    } else {
        resume(store, &parked_response, &parked_owner)
    };
    assert_eq!(
        refused["status"].as_str(),
        Some("retained_limit"),
        "{refused:?}"
    );
    assert!(
        store.read_hook.lock().unwrap().is_none(),
        "the filler never ran inside the step"
    );
    assert!(held.lock().unwrap().is_some());
    store.assert_conserved();
    assert!(audited(store)[TOTAL] <= cap_for(&foreground));
    Backstop {
        store,
        source,
        parked_owner,
        parked_response,
        slot,
        held,
    }
}

fn assert_discarded_build(store: &NativeStore, slot: &Arc<LoadSlot>) -> usize {
    let walk = cold_load_walk(slot);
    assert_eq!(
        walk,
        empty_index_walk(slot),
        "the refused build left buffers behind"
    );
    assert_eq!(
        slot.accounted.load(Ordering::Acquire),
        walk,
        "the slot charge is not its heap walked once"
    );
    {
        let state = store.lock_state();
        assert!(
            state.loads.values().any(|kept| Arc::ptr_eq(kept, slot)),
            "the refused build detached its slot"
        );
        assert!(!state.prepared_loads.contains_key(&slot.stamp.identity));
        assert_eq!(
            state.ledger.pending,
            NativeStore::audit_load_record_bytes(slot) + walk,
            "the pending gauge is not the slot record plus its heap"
        );
    }
    let load = slot.work.lock().unwrap();
    assert!(
        load.failure
            .as_ref()
            .is_some_and(|failure| failure.status == Status::RetainedLimit),
        "{:?}",
        load.failure
    );
    assert_eq!(
        (
            load.count,
            load.indexed,
            load.chunks.len(),
            load.pending.capacity(),
            load.fence.capacity(),
            load.origin_fence.capacity(),
            load.seal_fence.capacity(),
            load.prefix_fence.capacity(),
            load.session_id.is_none(),
        ),
        (0, 0, 0, 0, 0, 0, 0, 0, true),
        "the refused build kept part of what it built"
    );
    walk
}

#[test]
fn refused_unpublished_build_keeps_its_slot_charged_until_pruned() {
    let Backstop {
        store,
        source,
        parked_owner,
        slot,
        held,
        ..
    } = refused_backstop(false);
    assert!(store.lock_state().waiters.is_empty());
    let walk = assert_discarded_build(store, &slot);
    assert_eq!(
        Arc::strong_count(&slot),
        2,
        "the refused build is held outside the loads table"
    );
    assert_eq!(
        settled(store)[PENDING],
        NativeStore::audit_load_record_bytes(&slot) + walk,
        "the failed slot stopped carrying its record before it was pruned"
    );
    drop(slot);
    assert_eq!(
        settled(store)[PENDING],
        0,
        "the pruned slot left its record charged"
    );
    assert!(store.lock_state().loads.is_empty());
    drop(held.lock().unwrap().take());
    let retried = drive(
        store,
        store.request(
            &acquire(&source.path),
            &parked_owner,
            &Cancellation::default(),
        ),
        &parked_owner,
    );
    assert_eq!(retried["status"].as_str(), Some("ok"), "{retried:?}");
    assert_eq!(
        store
            .pin(&ok_handle(&retried), &parked_owner)
            .unwrap()
            .event_count,
        3
    );
    store.assert_conserved();
}

#[test]
fn refused_unpublished_build_fails_its_joined_waiter_once() {
    let Backstop {
        store,
        source,
        parked_owner,
        parked_response,
        slot,
        held,
    } = refused_backstop(true);
    {
        let state = store.lock_state();
        assert_eq!(
            state.waiters.len(),
            1,
            "the refusal removed more than its own waiter"
        );
        assert!(state.waiters.values().all(|waiter| {
            waiter.claimant == "backstop-parked" && Arc::ptr_eq(&waiter.load, &slot)
        }));
    }
    let walk = assert_discarded_build(store, &slot);
    let failed = resume(store, &parked_response, &parked_owner);
    assert_eq!(
        failed["status"].as_str(),
        Some("retained_limit"),
        "{failed:?}"
    );
    store.assert_conserved();
    assert!(
        store.lock_state().waiters.is_empty(),
        "the joined waiter outlived the failure it was handed"
    );
    assert_eq!(assert_discarded_build(store, &slot), walk);
    let stale = resume(store, &parked_response, &parked_owner);
    assert_eq!(stale["status"].as_str(), Some("stale_cursor"), "{stale:?}");
    store.assert_conserved();
    assert_eq!(
        settled(store)[PENDING],
        NativeStore::audit_load_record_bytes(&slot) + walk,
        "the failed slot stopped carrying its record before it was pruned"
    );
    drop(slot);
    assert_eq!(settled(store)[PENDING], 0);
    assert!(store.lock_state().loads.is_empty());
    drop(held.lock().unwrap().take());
    for owner in [parked_owner, context_for("backstop-joined", true)] {
        let retried = drive(
            store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(retried["status"].as_str(), Some("ok"), "{retried:?}");
        assert_eq!(
            store.pin(&ok_handle(&retried), &owner).unwrap().event_count,
            3
        );
        store.assert_conserved();
    }
}

#[test]
fn published_loads_release_their_decode_buffers() {
    let source = LedgerSource::new(&lines(0..4));
    for background in [false, true] {
        let requests = publishing_request(&source.path, background);
        let fixture = parked_load(&source.path, background, requests);
        let slot = pin_prepared_load(&fixture);
        {
            let load = slot.work.lock().unwrap();
            assert!(load.result.is_none() && !load.chunks.is_empty());
            assert!(load.chunks.capacity() * size_of::<Arc<EntryChunk>>() > 0);
        }
        let walk = cold_load_walk(&slot);
        assert_eq!(
            slot.accounted.load(Ordering::Acquire),
            walk,
            "the parked load's charge misses its chunk buffer"
        );
        let pending = fixture.store.lock_state().ledger.pending;
        assert_eq!(pending, NativeStore::audit_load_record_bytes(&slot) + walk);
        assert!(advance_published(&fixture));
        fixture.store.assert_conserved();
        let load = slot.work.lock().unwrap();
        assert!(load.result.is_some());
        assert_eq!(
            (
                load.pending.capacity(),
                load.chunks.capacity(),
                load.fence.capacity(),
                load.origin_fence.capacity(),
                load.seal_fence.capacity(),
                load.session_id.as_ref().map_or(0, String::capacity),
            ),
            (0, 0, 0, 0, 0, 0),
            "the published load kept its decode buffers"
        );
        assert_eq!(slot.accounted.load(Ordering::Acquire), 0);
    }
}

fn location_root_fixture(root: &Path, background: bool) -> Fixture {
    Fixture {
        store: cursor_store(),
        owner: context_for("locate-root", background),
        request: json!({"schema":SCHEMA,"id":"ledger-locate-root","operation":"locate","session_ids":["absent"],"roots":[root.to_string_lossy().as_ref()],"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        pins: Vec::new(),
    }
}

#[test]
fn location_cursor_charges_its_open_directory_root_copy() {
    let source = LedgerSource::new(&line("locate"));
    let roots = ["r".to_owned(), nested_name("r")].map(|name| {
        let root = source.directory.join(name);
        std::fs::create_dir_all(&root).unwrap();
        for file in ["a.jsonl", "b.jsonl", "c.jsonl"] {
            std::fs::write(root.join(file), line(file)).unwrap();
        }
        root
    });
    let site = "location park";
    for background in [false, true] {
        let [small, large] = [&roots[0], &roots[1]].map(|root| {
            exact_headroom(
                &|| location_root_fixture(root, background),
                &location_parked,
            )
        });
        assert_eq!(
            large - small,
            3 * 300,
            "{site}: the root is not reserved as the scope copy, the popped root, and the open directory's root copy"
        );
        let root = &roots[1];
        let build = || location_root_fixture(root, background);
        refused_at_site(site, &build, &location_parked, 0, large);
        let fitted = build();
        let capacity = fitted.store.lock_state().locates.capacity_bytes();
        let _filler = fill_to(&fitted.store, &fitted.owner, large);
        assert!(location_parked(&fitted));
        fitted.store.assert_conserved();
        let parked = {
            let state = fitted.store.lock_state();
            let (token, cursor) = state.locates.iter().next().expect("parked location cursor");
            assert_eq!(cursor.directories.len(), 1);
            assert_eq!(
                cursor.directories[0].root_capacity,
                root.as_os_str().len(),
                "{site}: the open directory does not record its root copy"
            );
            assert!(cursor.roots.is_empty());
            assert_eq!(cursor.scope.len(), 1);
            let parked = NativeStore::audit_locate_bytes(token, cursor)
                + state.locates.capacity_bytes()
                - capacity
                + state.locates.pledged(token);
            assert_eq!(
                large,
                location_step_peak(cursor, root).max(parked),
                "{site}: the step's admission is not its peak reservation or the cursor walk, its table growth, and its pledge"
            );
            parked
        };
        assert_eq!(
            ledger(&fitted.store)[TOTAL],
            cap_for(&fitted.owner) - large + parked,
            "{site}: the parked cursor is not charged as its walk, which holds the open directory's root copy"
        );
    }
}

#[test]
fn published_generations_record_their_arc_headers() {
    let source = LedgerSource::new(&lines(0..3));
    for background in [false, true] {
        let store = fast_store();
        let owner = context_for("headers", background);
        let (_, snapshot) = acquired(&store, &source.path, &owner);
        let state = store.lock_state();
        let record = state
            .generations
            .get(&snapshot_key(&snapshot))
            .expect("published generation");
        let (id, charge) = record.entries[0];
        assert_eq!(id, snapshot_key(&snapshot));
        assert_eq!(charge.opaque_dom_accounted_bytes, 0);
        assert_eq!(
            charge.owned_capacity_bytes,
            arc_mirror::<TranscriptSnapshot>()
                + snapshot.id.capacity()
                + snapshot.canonical_path.capacity()
                + snapshot.session_id.capacity()
                + snapshot.chunks.capacity() * size_of::<Arc<EntryChunk>>()
                + snapshot.fence.capacity(),
            "the generation's snapshot entry misses its arc header or path capacity"
        );
        assert_eq!(
            record.indexes[0],
            (
                Arc::as_ptr(&snapshot.activity) as usize,
                arc_mirror::<ActivityIndex>()
            ),
            "the generation's index entry misses its arc header"
        );
        assert_eq!(record.indexes, snapshot.activity.audited_allocations(true));
        assert_eq!(
            record
                .entries
                .iter()
                .map(|(id, charge)| {
                    (
                        *id,
                        charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
                    )
                })
                .collect::<Vec<_>>(),
            NativeStore::audit_snapshot_allocations(&snapshot)
        );
    }
}

fn listing_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("listing", background);
    let (root, root_snapshot) = acquired(&store, &source.path, &owner);
    Fixture {
        store,
        owner,
        request: prepare_request(&root, &[], &[], &[]),
        pins: vec![root_snapshot],
    }
}

#[test]
fn direct_listing_children_are_admitted_at_their_join_capacity() {
    let source = LedgerSource::new(&line("root"));
    let subagents = source.directory.join("s").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    for index in 0..12 {
        std::fs::write(
            subagents.join(format!("agent-{index:02}.jsonl")),
            line(&format!("agent-{index:02}")),
        )
        .unwrap();
    }
    let site = "direct listing";
    for background in [false, true] {
        let fixture = listing_fixture(&source, background);
        traced(&fixture.store);
        assert!(
            parked(&submit(&fixture)),
            "{site}: the listing finished in one step"
        );
        let trace = traced(&fixture.store);
        fixture.store.assert_conserved();
        let state = fixture.store.lock_state();
        let build = state.prepared_builds.values().next().expect("parked build");
        let listing = build.listing.as_ref().expect("parked inside the listing");
        assert!(!listing.children.is_empty());
        assert!(
            listing
                .children
                .iter()
                .any(|child| child.capacity() > child.as_os_str().len()),
            "{site}: no listed child outgrew its length"
        );
        let mut replay = Vec::<PathBuf>::new();
        let expected: Vec<usize> = listing
            .children
            .iter()
            .map(|child| {
                let before = replay.capacity();
                replay.push(child.clone());
                child.capacity() + (replay.capacity() - before) * size_of::<PathBuf>()
            })
            .collect();
        let reserved = reserved_traces(&trace);
        let mut next = 0;
        for bytes in &expected {
            let at = reserved[next..]
                .iter()
                .position(|entry| entry == bytes)
                .unwrap_or_else(|| {
                    panic!("{site}: child reservation {bytes} is missing in order: {reserved:?}")
                });
            next += at + 1;
        }
    }
}

fn assert_exact_sidechain_directories<'a>(
    site: &str,
    parents: impl Iterator<Item = &'a Path>,
    dirs: &[(PathBuf, Option<SourceStamp>)],
) {
    let expected: Vec<PathBuf> = parents
        .map(|parent| {
            parent
                .parent()
                .unwrap()
                .join(parent.file_stem().unwrap())
                .join("subagents")
        })
        .collect();
    assert!(
        dirs.iter().any(|(_, stamp)| stamp.is_none()),
        "{site}: no missing sidechain directory was recorded"
    );
    for (path, stamp) in dirs {
        assert_eq!(
            path.capacity(),
            path.as_os_str().len(),
            "{site}: {path:?} outgrew its length"
        );
        if stamp.is_none() {
            assert!(
                expected.contains(path),
                "{site}: {path:?} is not a member's sidechain directory"
            );
        }
    }
}

#[test]
fn sidechain_directories_are_built_at_exact_capacity() {
    let scenario = Scenario::new(3, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let fixture = direct_graph_fixture(&scenario, background);
        let response = submit(&fixture);
        assert!(admitted(&response, "ok"), "{response:?}");
        let graph_id = response["data"]["handle"]["graph_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[&graph_id].lock().unwrap();
        assert_exact_sidechain_directories(
            "direct build",
            graph.stamps.iter().map(|(path, _)| path.as_path()),
            &graph.sidechain_dirs,
        );
    }
    let warmed = warmed_registry(prepared_store(), &scaling_scenario(8));
    let state = warmed.store.lock_state();
    let membership = state
        .warm_memberships
        .values()
        .next()
        .expect("warmed membership");
    assert_exact_sidechain_directories(
        "warm membership",
        membership
            .members
            .iter()
            .map(|member| member.path.as_path()),
        &membership.sidechain_dirs,
    );
}

fn classifier_lease_fixture(source: &LedgerSource, background: bool, classifier: &str) -> Fixture {
    let mut fixture = lease_fixture(source, background, false);
    if classifier != "native" {
        fixture
            .store
            .register_classifier(
                classifier,
                "1",
                Arc::new(|_, range| Ok(vec![false; range.len()])),
            )
            .unwrap();
    }
    fixture.request = json!({"id":classifier,"version":"1"});
    fixture
}

fn retain_fixture(source: &LedgerSource, background: bool, classifier: &str) -> Fixture {
    let mut fixture = classifier_lease_fixture(source, background, classifier);
    let data = {
        let mut state = fixture.store.lock_state();
        fixture
            .store
            .issue(
                &mut state,
                Arc::clone(&fixture.pins[0]),
                fixture.request.clone(),
                &fixture.owner,
                now_ms() + 120_000,
                fixture.store.token("lease"),
                0,
            )
            .unwrap()
    };
    fixture.request = json!({"schema":SCHEMA,"id":"ledger-retain","operation":"retain","handle":data["description"]["handle"]});
    fixture
}

fn lease_walk(store: &NativeStore) -> usize {
    store
        .lock_state()
        .leases
        .audit_with(NativeStore::audit_lease_bytes)
}

#[test]
fn lease_admits_and_charges_its_classifier() {
    let source = LedgerSource::new(&lines(0..2));
    let long = "c".repeat(4096);
    let retained = |fixture: &Fixture| admitted(&submit(fixture), "ok");
    for background in [false, true] {
        let issue_exact = |classifier: &str| {
            exact_headroom(
                &|| classifier_lease_fixture(&source, background, classifier),
                &issued,
            )
        };
        let retain_exact = |classifier: &str| {
            exact_headroom(
                &|| retain_fixture(&source, background, classifier),
                &retained,
            )
        };
        assert_eq!(
            issue_exact(long.as_str()) - issue_exact("native"),
            long.len() - "native".len(),
            "lease issue omits its classifier"
        );
        assert_eq!(
            retain_exact(long.as_str()) - retain_exact("native"),
            long.len() - "native".len(),
            "lease retain omits its cloned classifier"
        );
        for classifier in ["native", long.as_str()] {
            let build = || classifier_lease_fixture(&source, background, classifier);
            let exact = issue_exact(classifier);
            refused_at_site("lease issue classifier", &build, &issued, 0, exact);
            let fitted = build();
            let walked = lease_walk(&fitted.store);
            let capacity = fitted.store.lock_state().leases.capacity_bytes();
            let _filler = fill_to(&fitted.store, &fitted.owner, exact);
            assert!(issued(&fitted));
            fitted.store.assert_conserved();
            assert_eq!(ledger(&fitted.store)[TOTAL], cap_for(&fitted.owner));
            let grown = fitted.store.lock_state().leases.capacity_bytes() - capacity;
            assert_eq!(
                exact,
                lease_walk(&fitted.store) - walked + grown,
                "lease issue is not admitted at its walked record and table growth"
            );
            let build = || retain_fixture(&source, background, classifier);
            let exact = retain_exact(classifier);
            refused_at_site(
                "lease retain classifier",
                &build,
                &retained,
                REPLY_RESERVATION,
                exact,
            );
            let fitted = build();
            let walked = lease_walk(&fitted.store);
            let capacity = fitted.store.lock_state().leases.capacity_bytes();
            let before = delivered(&fitted.store);
            let _filler = fill_to(&fitted.store, &fitted.owner, exact);
            assert!(retained(&fitted));
            fitted.store.assert_conserved();
            let grown = fitted.store.lock_state().leases.capacity_bytes() - capacity;
            assert_eq!(
                exact,
                REPLY_RESERVATION + lease_walk(&fitted.store) - walked
                    + grown
                    + delivered(&fitted.store)
                    - before,
                "retain is not admitted at its lease, table growth, and delivery"
            );
        }
    }
}

fn resolution_charge_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let store = fast_store();
    let owner = restricted_context("resolve", background, &scenario.root.directory, padding);
    let pins = scenario
        .sidechains
        .iter()
        .map(|path| acquired(&store, path, &owner).1)
        .collect();
    Fixture {
        store,
        request: json!({"schema":SCHEMA,"id":"ledger-resolve","operation":"resolve","session_ids":scenario.ids(),"roots":scenario.roots(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        owner,
        pins,
    }
}

#[test]
fn resolution_cursor_charges_and_admits_its_context_and_nested_tables() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    let authority = |padding: usize| {
        value_bytes(
            &restricted_context("resolve", false, &scenario.root.directory, padding)["authority"],
        )
    };
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let build = |padding: usize| resolution_charge_fixture(&scenario, background, padding);
        let retained = |padding: usize| {
            let fixture = build(padding);
            let before = settled(&fixture.store)[TOTAL];
            assert!(attempt(&fixture));
            fixture.store.assert_conserved();
            settled(&fixture.store)[TOTAL] - before
        };
        assert_eq!(
            retained(63) - retained(0),
            authority(63) - authority(0),
            "the parked resolution omits its context"
        );
        let small = exact_headroom(&|| build(0), &attempt);
        let large = exact_headroom(&|| build(63), &attempt);
        assert_eq!(
            large - small,
            authority(63) - authority(0),
            "the resolution park admission omits its context"
        );
        for (padding, exact) in [(0, small), (63, large)] {
            refused_at_site(
                "resolution park",
                &|| build(padding),
                &attempt,
                REPLY_RESERVATION,
                exact,
            );
        }
        let fixture = build(63);
        assert!(attempt(&fixture));
        let state = fixture.store.lock_state();
        let (token, cursor) = state.resolutions.iter().next().expect("parked resolution");
        assert!(
            cursor.ids.capacity() >= 2
                && cursor.paths.capacity() >= 2
                && cursor.sessions.capacity() >= 1,
            "the resolution has no nested buffers"
        );
        assert!(cursor.pending.is_none());
        let nested = cursor.ids.capacity() * size_of::<String>()
            + cursor.ids.iter().map(String::capacity).sum::<usize>()
            + cursor.paths.capacity() * size_of::<(String, PathBuf)>()
            + cursor
                .paths
                .iter()
                .map(|(id, path)| id.capacity() + path.capacity())
                .sum::<usize>()
            + cursor.sessions.capacity() * size_of::<Value>()
            + cursor.sessions.iter().map(value_bytes).sum::<usize>();
        assert_eq!(
            state.resolutions.charged(),
            token.capacity()
                + cursor.claimant.capacity()
                + value_bytes(&cursor.context)
                + value_bytes(&cursor.request)
                + nested
                + state.resolutions.pledged(token),
            "the resolution charge is not its key, strings, context, request, and nested table, vector, and key capacities"
        );
    }
}

#[test]
fn waiter_context_rebind_admits_its_growth_before_replacing_the_context() {
    let source = LedgerSource::new(&lines(0..8));
    let padded = padded_context;
    let rebound = |fixture: &Fixture| {
        let token = fixture.request["cursor"].as_str().unwrap();
        match fixture.store.rebind_waiter(
            &mut fixture.store.lock_state(),
            token,
            &padded(&fixture.owner),
        ) {
            Ok(Some(_)) => true,
            Ok(None) => panic!("the parked waiter vanished"),
            Err(error) if error.status == Status::RetainedLimit => false,
            Err(error) => panic!("waiter rebind failed outside admission: {error:?}"),
        }
    };
    for background in [false, true] {
        let build = || parked_load(&source.path, background, 2);
        let probe = build();
        let growth = value_bytes(&padded(&probe.owner)) - value_bytes(&probe.owner);
        assert!(growth >= 4096);
        assert_eq!(
            exact_headroom(&build, &rebound),
            growth,
            "the rebind is not admitted at its context growth"
        );
        for headroom in [0, growth - 1] {
            let refused = build();
            let token = refused.request["cursor"].as_str().unwrap().to_owned();
            assert_refused_at("waiter rebind", &refused, &rebound, headroom + 1);
            assert_eq!(
                refused.store.lock_state().waiters[&token].context,
                refused.owner,
                "a refused rebind replaced the context"
            );
        }
        let fitted = build();
        let walked = fitted
            .store
            .lock_state()
            .waiters
            .audit_with(NativeStore::audit_waiter_bytes);
        let _filler = fill_to(&fitted.store, &fitted.owner, growth);
        assert!(rebound(&fitted));
        fitted.store.assert_conserved();
        assert_eq!(ledger(&fitted.store)[TOTAL], cap_for(&fitted.owner));
        assert_eq!(
            fitted
                .store
                .lock_state()
                .waiters
                .audit_with(NativeStore::audit_waiter_bytes)
                - walked,
            growth
        );
        let resumed = build();
        let token = resumed.request["cursor"].as_str().unwrap().to_owned();
        {
            let _filler = fill_to(
                &resumed.store,
                &resumed.owner,
                REPLY_RESERVATION + growth - 1,
            );
            let response = resumed.store.request(
                &resume_request(&token),
                &padded(&resumed.owner),
                &Cancellation::default(),
            );
            assert_eq!(
                response["status"].as_str(),
                Some("retained_limit"),
                "{response:?}"
            );
        }
        assert_eq!(
            resumed.store.lock_state().waiters[&token].context,
            resumed.owner,
            "the resume replaced the context without admitting it"
        );
        resumed.store.assert_conserved();
    }
}

fn projection_fixture(source: &LedgerSource, background: bool, claimant: &str) -> Fixture {
    let store = cursor_store();
    let owner = context_for(claimant, background);
    let (handle, snapshot) = acquired(&store, &source.path, &owner);
    Fixture {
        store,
        request: json!({"schema":SCHEMA,"id":"ledger-prompts","operation":"query","view":{"handle":handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"query":{"kind":"prompts","selection":"first","count":10},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        owner,
        pins: vec![snapshot],
    }
}

#[test]
fn projection_cursor_charges_its_identity_strings() {
    let source = LedgerSource::new(&lines(0..3));
    let long = "p".repeat(4096);
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let exact_for = |claimant: &str| {
            exact_headroom(
                &|| projection_fixture(&source, background, claimant),
                &attempt,
            )
        };
        assert_eq!(
            exact_for(long.as_str()) - exact_for("p"),
            2 * (long.len() - 1),
            "the projection cursor omits its claimant"
        );
        for claimant in ["p", long.as_str()] {
            let build = || projection_fixture(&source, background, claimant);
            let exact = exact_for(claimant);
            refused_at_site(
                "projection park",
                &build,
                &attempt,
                REPLY_RESERVATION,
                exact,
            );
            let fitted = build();
            let capacity = fitted.store.lock_state().projections.capacity_bytes();
            let before = delivered(&fitted.store);
            let _filler = fill_to(&fitted.store, &fitted.owner, exact);
            assert!(attempt(&fitted));
            fitted.store.assert_conserved();
            let walked = {
                let state = fitted.store.lock_state();
                let (token, cursor) = state.projections.iter().next().expect("parked projection");
                NativeStore::audit_projection_bytes(token, cursor)
                    + state.projections.capacity_bytes()
                    - capacity
            };
            assert_eq!(
                exact,
                REPLY_RESERVATION + walked + delivered(&fitted.store) - before,
                "the projection park is not admitted at its cursor, table growth, and delivery"
            );
        }
    }
}

fn discovery_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let mut owner = context_for("discover", background);
    owner.insert("padding", json!("p".repeat(padding)));
    Fixture {
        store: cursor_store(),
        owner,
        request: json!({"schema":SCHEMA,"id":"ledger-discover","operation":"discover","roots":scenario.roots(),"checkpoint":null,"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        pins: Vec::new(),
    }
}

#[test]
fn discovery_cursor_and_checkpoint_charge_their_context_request_and_tables() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let attempt = |fixture: &Fixture| parked(&submit(fixture));
    for background in [false, true] {
        let build = |padding: usize| discovery_fixture(&scenario, background, padding);
        let small = exact_headroom(&|| build(0), &attempt);
        let large = exact_headroom(&|| build(4096), &attempt);
        assert_eq!(
            large - small,
            4096,
            "the discovery park admission omits its context"
        );
        for (padding, exact) in [(0, small), (4096, large)] {
            refused_at_site(
                "discovery park",
                &|| build(padding),
                &attempt,
                REPLY_RESERVATION,
                exact,
            );
            let fitted = build(padding);
            let capacity = fitted.store.lock_state().discoveries.capacity_bytes();
            let before = delivered(&fitted.store);
            let _filler = fill_to(&fitted.store, &fitted.owner, exact);
            assert!(attempt(&fitted));
            fitted.store.assert_conserved();
            let (walked, peak) = {
                let state = fitted.store.lock_state();
                let (token, scan) = state.discoveries.iter().next().expect("parked discovery");
                assert!(
                    scan.inventory.capacity() > 0 && scan.seen.capacity() > 0,
                    "the discovery has no nested tables"
                );
                (
                    NativeStore::audit_discovery_bytes(token, scan)
                        + state.discoveries.capacity_bytes()
                        - capacity,
                    discovery_step_peak(token, scan, &scenario.root.directory),
                )
            };
            assert_eq!(
                exact,
                REPLY_RESERVATION + peak.max(walked + delivered(&fitted.store) - before),
                "the discovery step's admission is not its peak reservation or its parked record"
            );
        }
        let completed = build(4096);
        let settled_reply = drive(&completed.store, submit(&completed), &completed.owner);
        assert_eq!(
            settled_reply["status"].as_str(),
            Some("ok"),
            "{settled_reply:?}"
        );
        let state = completed.store.lock_state();
        let (token, checkpoint) = state
            .checkpoints
            .iter()
            .next()
            .expect("discovery checkpoint");
        assert_eq!(checkpoint.inventory.len(), 10);
        assert_eq!(
            state.checkpoints.charged(),
            token.capacity()
                + checkpoint.claimant.capacity()
                + value_bytes(&checkpoint.roots)
                + checkpoint.inventory.capacity() * size_of::<(String, Value)>()
                + checkpoint
                    .inventory
                    .iter()
                    .map(|(path, value)| path.capacity() + value_bytes(value))
                    .sum::<usize>(),
            "the checkpoint charge is not its claimant, roots, inventory tier, and measured entries"
        );
    }
}

#[test]
fn graph_cursor_charges_its_context_and_nested_buffers() {
    let source = LedgerSource::new(&line("root"));
    let children = source.directory.join("s/subagents");
    std::fs::create_dir_all(&children).unwrap();
    for index in 0..9 {
        std::fs::write(
            children.join(format!("agent-{index}.jsonl")),
            line(&format!("agent-{index}")),
        )
        .unwrap();
    }
    for background in [false, true] {
        let parked_graph = |padding: usize| {
            let store = cursor_store();
            let mut owner = context_for("graph", background);
            owner.insert("padding", json!("p".repeat(padding)));
            let (root, _snapshot) = acquired(&store, &source.path, &owner);
            let before = settled(&store)[TOTAL];
            let response = store.request(
                &json!({"schema":SCHEMA,"id":"ledger-graph","operation":"query","view":{"handle":root,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"query":missing_tool(),"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
                &owner,
                &Cancellation::default(),
            );
            assert!(parked(&response), "{response:?}");
            store.assert_conserved();
            let holders = {
                let state = store.lock_state();
                assert_eq!(state.graphs.len(), 1);
                let (_, graph) = state.graphs.iter().next().unwrap();
                assert!(graph.seen.capacity() > 0 && graph.nodes.capacity() > 0);
                1 + state.waiters.len()
            };
            (settled(&store)[TOTAL] - before, holders)
        };
        let (small, holders) = parked_graph(0);
        let (large, large_holders) = parked_graph(4096);
        assert_eq!(holders, large_holders);
        assert_eq!(
            large - small,
            4096 * holders,
            "a parked graph or its source waiter omits the context"
        );
    }
}

fn released_seed_label(background: bool) -> (LedgerSource, NativeStore, Value, String, LabelSlot) {
    let source = LedgerSource::new(&lines(0..4));
    let store = fast_store();
    let owner = context_for("seeded-labels", background);
    recording_classifier(&store, "overlap");
    classified(&store, &source.path, "overlap", &owner);
    source.append(&lines(4..300));
    let (native, _) = acquired(&store, &source.path, &owner);
    let page = store
        .prepare_classifier(
            &native,
            &json!({"id":"overlap","version":"1"}),
            &owner,
            &Cancellation::default(),
            label_bounds(),
        )
        .unwrap();
    assert_eq!(
        page["event_start"].as_u64(),
        Some(4),
        "the label preparation was not seeded: {page:?}"
    );
    let token = page["cursor"].as_str().unwrap().to_owned();
    let slot = {
        let mut state = store.lock_state();
        let lineage = state
            .carried_classifications
            .iter()
            .next()
            .map(|(lineage, _)| lineage.clone())
            .expect("carried classification");
        state.remove_carried(&lineage);
        state.remove_label(&token).expect("published label slot")
    };
    (source, store, owner, token, slot)
}

#[test]
fn label_republication_reserves_seed_anchors_its_carried_record_released() {
    for background in [false, true] {
        let publish = |bytes: usize| {
            let (_source, store, owner, token, slot) = released_seed_label(background);
            let before = (ledger(&store), audited(&store), bookkeeping(&store));
            let mut reservation = store.reserve_projection(&owner, bytes).unwrap();
            let published = match store.publish_label(token.clone(), slot, &owner, &mut reservation)
            {
                Ok(()) => true,
                Err(error) if error.status == Status::RetainedLimit => false,
                Err(error) => panic!("label publication failed outside admission: {error:?}"),
            };
            drop(reservation);
            store.assert_conserved();
            if !published {
                assert!(
                    !store.lock_state().labels.contains_key(&token),
                    "a refused label slot was published"
                );
                assert_eq!(
                    (ledger(&store), audited(&store), bookkeeping(&store)),
                    before,
                    "the refused publication leaked state"
                );
            }
            (published, store, token)
        };
        let (fitted, store, token) = publish(SEARCH_LIMIT);
        assert!(fitted);
        let exact = {
            let state = store.lock_state();
            let slot = &state.labels[&token];
            let seed = slot.preparation.seed().expect("seeded label slot");
            for (id, _) in seed.activity.audited_allocations(true) {
                assert!(
                    state
                        .generations
                        .values()
                        .any(|record| { record.indexes.iter().any(|(owned, _)| *owned == id) }),
                    "the seed index is not owned by its generation"
                );
            }
            NativeStore::audit_label_bytes(&token, slot)
                + arc_mirror::<CarriedClassification>()
                + seed.prefix.capacity() * size_of::<Arc<EntryChunk>>()
        };
        assert!(!publish(0).0);
        assert!(
            !publish(exact - 1).0,
            "the seed payload released by the carried record was not reserved"
        );
        assert!(publish(exact).0, "the exact reservation was refused");
    }
}

fn padded_context(owner: &Value) -> Value {
    let mut padded = owner.clone();
    padded.insert("padding", json!("p".repeat(4096)));
    padded
}

#[test]
fn busy_waiter_rebind_leaves_its_context_and_charge_in_place() {
    let source = LedgerSource::new(&lines(0..8));
    for background in [false, true] {
        let fixture = parked_load(&source.path, background, 2);
        let token = fixture.request["cursor"].as_str().unwrap().to_owned();
        let large = padded_context(&fixture.owner);
        let growth = value_bytes(&large) - value_bytes(&fixture.owner);
        assert!(matches!(
            fixture
                .store
                .rebind_waiter(&mut fixture.store.lock_state(), &token, &large),
            Ok(Some(_))
        ));
        fixture
            .store
            .lock_state()
            .waiters
            .get_mut(&token)
            .expect("parked waiter")
            .busy = true;
        let _filler = fill_to(&fixture.store, &fixture.owner, 0);
        let before = ledger(&fixture.store);
        let walked = fixture
            .store
            .lock_state()
            .waiters
            .audit_with(NativeStore::audit_waiter_bytes);
        {
            let mut state = fixture.store.lock_state();
            let rebound = fixture
                .store
                .rebind_waiter(&mut state, &token, &fixture.owner)
                .unwrap()
                .expect("busy waiter");
            assert_eq!(
                rebound.context, large,
                "the rebind returned a context its owner is not running"
            );
            assert_eq!(
                state.waiters[&token].context, large,
                "a busy waiter's context was replaced"
            );
        }
        assert_eq!(
            ledger(&fixture.store),
            before,
            "a busy rebind moved the ledger"
        );
        assert_eq!(
            fixture
                .store
                .lock_state()
                .waiters
                .audit_with(NativeStore::audit_waiter_bytes),
            walked
        );
        assert_eq!(
            fixture
                .store
                .reserve_projection(&fixture.owner, 1)
                .err()
                .map(|error| error.status),
            Some(Status::RetainedLimit),
            "an admission fit into a busy waiter's context bytes"
        );
        {
            let mut state = fixture.store.lock_state();
            let mut returned = state.waiters[&token].clone();
            returned.busy = false;
            state.waiters.insert(token.clone(), returned);
        }
        assert_eq!(ledger(&fixture.store), before);
        fixture.store.assert_conserved();
        assert!(matches!(
            fixture
                .store
                .rebind_waiter(&mut fixture.store.lock_state(), &token, &fixture.owner),
            Ok(Some(_))
        ));
        assert_eq!(
            before[TOTAL] - ledger(&fixture.store)[TOTAL],
            growth,
            "an idle rebind did not release the context difference"
        );
        assert_eq!(
            fixture.store.lock_state().waiters[&token].context,
            fixture.owner
        );
        fixture.store.assert_conserved();
    }
}

#[test]
fn concurrent_resume_cannot_rebind_a_busy_waiter() {
    let source = LedgerSource::new(&lines(0..8));
    for background in [false, true] {
        let store = Arc::new(slow_store());
        let owner = context_for("busy-resume", background);
        let parked = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        assert_eq!(parked["status"].as_str(), Some("incomplete"), "{parked:?}");
        let token = parked["cursor"].as_str().unwrap().to_owned();
        let large = padded_context(&owner);
        let observed = Arc::new(Mutex::new(None));
        *store.read_hook.lock().unwrap() = Some(Arc::new({
            let store = Arc::clone(&store);
            let owner = owner.clone();
            let token = token.clone();
            let observed = Arc::clone(&observed);
            move || {
                let before = {
                    let state = store.lock_state();
                    (
                        state.waiters.audit_with(NativeStore::audit_waiter_bytes),
                        state.waiters.charged(),
                    )
                };
                let response =
                    store.request(&resume_request(&token), &owner, &Cancellation::default());
                let state = store.lock_state();
                *observed.lock().unwrap() = Some((
                    response,
                    state.waiters[&token].context.clone(),
                    before,
                    (
                        state.waiters.audit_with(NativeStore::audit_waiter_bytes),
                        state.waiters.charged(),
                    ),
                ));
            }
        }));
        let outer = store.request(&resume_request(&token), &large, &Cancellation::default());
        assert_eq!(outer["status"].as_str(), Some("incomplete"), "{outer:?}");
        let (response, stored, before, after) = observed
            .lock()
            .unwrap()
            .take()
            .expect("the concurrent resume never ran");
        assert_eq!(
            response["reason"].as_str(),
            Some("reservation preparation is running"),
            "{response:?}"
        );
        assert_eq!(
            stored, large,
            "the concurrent resume replaced a busy waiter's context"
        );
        assert_eq!(
            after, before,
            "the concurrent resume moved a busy waiter's charge"
        );
        assert_eq!(store.lock_state().waiters[&token].context, large);
        store.assert_conserved();
    }
}

fn sole_owner_label(
    background: bool,
) -> (
    LedgerSource,
    NativeStore,
    Value,
    String,
    Vec<Arc<TranscriptSnapshot>>,
    Arc<CarriedClassification>,
) {
    let source = LedgerSource::new(&lines(0..4));
    let store = fast_store();
    let owner = context_for("seeded-labels", background);
    recording_classifier(&store, "overlap");
    let (_, first) = classified(&store, &source.path, "overlap", &owner);
    source.append(&lines(4..300));
    let (native, latest) = acquired(&store, &source.path, &owner);
    let page = store
        .prepare_classifier(
            &native,
            &json!({"id":"overlap","version":"1"}),
            &owner,
            &Cancellation::default(),
            label_bounds(),
        )
        .unwrap();
    assert_eq!(
        page["event_start"].as_u64(),
        Some(4),
        "the label preparation was not seeded: {page:?}"
    );
    let token = page["cursor"].as_str().unwrap().to_owned();
    let (_, newer) = classified(&store, &source.path, "overlap", &owner);
    let carried = {
        let mut state = store.lock_state();
        let seed = Arc::clone(
            state.labels[&token]
                .preparation
                .seed()
                .expect("seeded label slot"),
        );
        let seeded_stage = state
            .classifier_stages
            .iter()
            .find(|(_, slot)| {
                slot.seed
                    .as_ref()
                    .is_some_and(|held| Arc::ptr_eq(held, &seed))
            })
            .map(|(key, _)| key.clone());
        if let Some(key) = seeded_stage {
            state.remove_classifier_stage(&key);
        }
        let carried = state
            .carried_classifications
            .values()
            .next()
            .map(Arc::clone)
            .expect("carried classification");
        assert!(
            !Arc::ptr_eq(&carried, &seed),
            "the newer classification did not replace the carried record"
        );
        assert!(
            state.classifier_stages.values().all(|slot| slot
                .seed
                .as_ref()
                .is_none_or(|held| !Arc::ptr_eq(held, &seed))),
            "a classifier stage still owns the label's seed"
        );
        carried
    };
    (
        source,
        store,
        owner,
        token,
        vec![first, latest, newer],
        carried,
    )
}

#[test]
fn label_extraction_carries_the_seed_anchors_it_last_owned() {
    for background in [false, true] {
        let (_source, store, owner, token, _pins, _carried) = sole_owner_label(background);
        let (payload, walked, pledged) = {
            let state = store.lock_state();
            let slot = &state.labels[&token];
            let seed = slot.preparation.seed().expect("seeded label slot");
            for (id, _) in seed.activity.audited_allocations(true) {
                assert!(
                    state
                        .generations
                        .values()
                        .any(|record| record.indexes.iter().any(|(owned, _)| *owned == id)),
                    "the seed index is not owned by its generation"
                );
            }
            (
                arc_mirror::<CarriedClassification>()
                    + seed.prefix.capacity() * size_of::<Arc<EntryChunk>>(),
                NativeStore::audit_label_bytes(&token, slot),
                state.labels.pledged(&token),
            )
        };
        let _filler = fill_to(&store, &owner, 0);
        let before = ledger(&store);
        let mut reservation = store.reserve_projection(&owner, 0).unwrap();
        let slot = store
            .lock_state()
            .extract_label(&token, &mut reservation)
            .expect("published label slot");
        store.assert_conserved();
        let during = ledger(&store);
        assert_eq!(
            before[TOTAL] - during[TOTAL],
            pledged,
            "label extraction released more than its delivery pledge while its seed is live"
        );
        assert_eq!(
            before[INDEXES] - during[INDEXES],
            payload,
            "the extracted seed payload did not leave the shared index gauge"
        );
        assert_eq!(
            store
                .reserve_projection(&owner, pledged + 1)
                .err()
                .map(|error| error.status),
            Some(Status::RetainedLimit),
            "an admission fit into the extracted seed's bytes"
        );
        drop(slot);
        drop(reservation);
        store.assert_conserved();
        assert_eq!(
            before[TOTAL] - ledger(&store)[TOTAL],
            walked + pledged + payload,
            "dropping the extracted label released other than its walk, pledge, and seed payload"
        );
    }
}

fn reached(probe: fn(&NativeStore) -> usize, run: impl Fn(&Fixture)) -> impl Fn(&Fixture) -> bool {
    move |fixture: &Fixture| {
        let before = probe(&fixture.store);
        run(fixture);
        probe(&fixture.store) > before
    }
}

fn settled_or_refused(response: &Value) {
    assert!(
        matches!(
            response["status"].as_str(),
            Some("ok" | "incomplete" | "retained_limit")
        ),
        "{response:?}"
    );
}

fn reserved(store: &NativeStore) -> Vec<usize> {
    reserved_traces(&traced(store))
}

fn subagent_graph_source() -> LedgerSource {
    let source = LedgerSource::new(&line("root"));
    let children = source.directory.join("s/subagents");
    std::fs::create_dir_all(&children).unwrap();
    for index in 0..9 {
        std::fs::write(
            children.join(format!("agent-{index}.jsonl")),
            line(&format!("agent-{index}")),
        )
        .unwrap();
    }
    source
}

fn graph_fixture(source: &LedgerSource, background: bool, attachments: Value) -> Fixture {
    let store = cursor_store();
    let owner = context_for("graph-admission", background);
    let (root, snapshot) = acquired(&store, &source.path, &owner);
    Fixture {
        request: json!({"schema":SCHEMA,"id":"ledger-graph","operation":"query","view":{"handle":root,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":attachments},"query":missing_tool(),"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        store,
        owner,
        pins: vec![snapshot],
    }
}

fn graph_record_bytes(graph: &GraphCursor) -> usize {
    let root = &graph.nodes[0];
    size_of::<GraphCursor>()
        + graph.claimant.capacity()
        + value_bytes(&graph.context)
        + value_bytes(&graph.request)
        + value_bytes(&graph.root_handle)
        + graph.nodes.capacity() * size_of::<GraphNode>()
        + root.path.capacity()
        + value_bytes(&root.description)
        + graph.seen.capacity() * size_of::<SourceIdentity>()
        + graph.tasks.capacity() * size_of::<GraphTask>()
        + root.path.capacity()
}

fn locate_record_bytes(cursor: &LocateCursor) -> usize {
    size_of::<LocateCursor>()
        + cursor.claimant.capacity()
        + value_bytes(&cursor.context)
        + cursor.ids.capacity() * size_of::<String>()
        + cursor.ids.iter().map(String::capacity).sum::<usize>()
        + cursor.wanted.capacity() * size_of::<String>()
        + cursor.wanted.iter().map(String::capacity).sum::<usize>()
        + cursor.scope.capacity() * size_of::<PathBuf>()
        + 2 * cursor.scope.iter().map(PathBuf::capacity).sum::<usize>()
        + cursor.roots.capacity() * size_of::<PathBuf>()
}

#[test]
fn fresh_graph_cursor_admits_its_record_before_constructing_it() {
    let source = subagent_graph_source();
    let directory = std::fs::canonicalize(&source.directory)
        .unwrap()
        .join("s")
        .join("subagents");
    let listed_directory =
        2 * directory.as_os_str().len() + arc_mirror::<(*mut libc::DIR, PathBuf)>();
    for background in [false, true] {
        let build = || graph_fixture(&source, background, json!([]));
        let control = build();
        let capacity = control.store.lock_state().graphs.capacity_bytes();
        let delivered_before = delivered(&control.store);
        traced(&control.store);
        assert!(parked(&submit(&control)));
        let reservations = reserved(&control.store);
        assert!(
            reservations.contains(&listed_directory),
            "the graph listing did not reserve its directory path, root copy, and open-directory handle in one extension: {reservations:?}"
        );
        let (record, peak, stored) = {
            let state = control.store.lock_state();
            let (token, graph) = state.graphs.iter().next().expect("parked graph");
            assert_eq!((graph.nodes.len(), graph.nodes.capacity()), (1, 1));
            assert!(graph.tasks.is_empty());
            assert_eq!(graph.tasks.capacity(), 1);
            let listing = graph.listing.as_ref().expect("parked listing");
            assert_eq!(listing.children.len(), 1);
            let record = graph_record_bytes(graph);
            (
                record,
                record
                    + FILESYSTEM_PATH_BYTES
                    + listed_directory
                    + listing.children.capacity() * size_of::<PathBuf>()
                    + listing.children[0].capacity(),
                NativeStore::audit_graph_cursor_bytes(token, graph) + state.graphs.capacity_bytes()
                    - capacity,
            )
        };
        let parked_bytes = stored + delivered(&control.store) - delivered_before;
        let site = reached(
            |store| store.graph_records.load(Ordering::Relaxed),
            |fixture| settled_or_refused(&submit(fixture)),
        );
        let exact = exact_headroom(&build, &site);
        assert_eq!(
            exact,
            REPLY_RESERVATION + record,
            "the fresh graph cursor's reservation is not its constructed record"
        );
        for headroom in [REPLY_RESERVATION, exact - 1] {
            let fixture = build();
            let leases = lease_table(&fixture.store);
            let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
            traced(&fixture.store);
            assert!(
                !site(&fixture),
                "the graph record was built before its admission"
            );
            assert_eq!(reserved(&fixture.store), vec![REPLY_RESERVATION]);
            assert!(fixture.store.lock_state().graphs.is_empty());
            assert_eq!(lease_table(&fixture.store), leases);
        }
        refused_at_site(
            "fresh graph record",
            &build,
            &site,
            REPLY_RESERVATION,
            exact,
        );
        let step = exact_headroom(&build, &|fixture: &Fixture| parked(&submit(fixture)));
        assert_eq!(
            step,
            REPLY_RESERVATION + peak.max(parked_bytes),
            "the graph step's admission is not its peak reservation or its parked record"
        );
        assert_boundary_at(
            "graph step",
            &build(),
            &|fixture: &Fixture| parked(&submit(fixture)),
            step,
        );
    }
}

#[test]
fn graph_source_admits_its_node_before_pinning_it() {
    let source = subagent_graph_source();
    let member = std::fs::canonicalize(source.directory.join("s/subagents/agent-0.jsonl")).unwrap();
    for background in [false, true] {
        let setup = || {
            let fixture = graph_fixture(&source, background, json!([]));
            assert!(parked(&submit(&fixture)));
            let (_, pin) = acquired(&fixture.store, &member, &fixture.owner);
            let graph = {
                let mut state = fixture.store.lock_state();
                let token = state
                    .graphs
                    .iter()
                    .next()
                    .map(|(token, _)| token.clone())
                    .expect("parked graph");
                state.graphs.remove(&token).expect("parked graph")
            };
            let (data, pending, _) = fixture
                .store
                .acquire(
                    &acquire(&member),
                    &graph.context,
                    &Cancellation::default(),
                    &mut [0u64; 18],
                )
                .unwrap();
            assert!(pending.is_none(), "the member acquire was not a cache hit");
            (fixture, graph, data, pin)
        };
        let add = |headroom: usize| {
            let (fixture, mut graph, data, pin) = setup();
            let before = (
                graph.nodes.capacity(),
                graph.seen.capacity(),
                graph.tasks.capacity(),
            );
            let added = {
                let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
                let mut reservation = fixture.store.reserve_projection(&fixture.owner, 0).unwrap();
                fixture.store.graph_add_source(
                    &mut graph,
                    member.clone(),
                    1,
                    Some("0".to_owned()),
                    &data,
                    &mut reservation,
                )
            };
            (fixture, graph, data, pin, before, added)
        };
        let delta = {
            let (_fixture, graph, _data, _pin, before, added) = add(SEARCH_LIMIT);
            added.unwrap();
            let node = graph.nodes.last().expect("added node");
            (graph.nodes.capacity() - before.0) * size_of::<GraphNode>()
                + node.path.capacity()
                + value_bytes(&node.description)
                + (graph.seen.capacity() - before.1) * size_of::<SourceIdentity>()
                + (graph.tasks.capacity() - before.2) * size_of::<GraphTask>()
        };
        let exact = exact_fit(&|headroom| match add(headroom).5 {
            Ok(()) => true,
            Err(error) if error.status == Status::RetainedLimit => false,
            Err(error) => panic!("graph source failed outside admission: {error:?}"),
        });
        assert_eq!(
            exact, delta,
            "the member node was not admitted by its stored bytes"
        );
        for headroom in [0, exact - 1] {
            let (refused, graph, _data, _pin, _, added) = add(headroom);
            assert_eq!(added.unwrap_err().status, Status::RetainedLimit);
            assert_eq!(
                refused.store.graph_sources.load(Ordering::Relaxed),
                0,
                "the member was pinned before its node was admitted"
            );
            assert_eq!(graph.nodes.len(), 1);
            let (control, _, data, _control_pin) = setup();
            control
                .store
                .lock_state()
                .leases
                .remove(str_field(&data["description"]["handle"], "lease_id").unwrap());
            evict_unpinned(&control.store, &control.owner);
            assert_eq!(lease_table(&refused.store), lease_table(&control.store));
            assert_eq!(
                (
                    settled(&refused.store),
                    audited(&refused.store),
                    bookkeeping(&refused.store)
                ),
                (
                    settled(&control.store),
                    audited(&control.store),
                    bookkeeping(&control.store)
                ),
                "a refused member admission leaked its lease or state"
            );
        }
    }
}

fn pending_graph_fixture(
    source: &LedgerSource,
    attachment: &Path,
    background: bool,
    padding: usize,
) -> Fixture {
    let mut fixture = graph_fixture(
        source,
        background,
        json!([attachment.to_string_lossy().as_ref()]),
    );
    let first = submit(&fixture);
    assert!(parked(&first));
    assert!(
        fixture
            .store
            .lock_state()
            .graphs
            .values()
            .all(|graph| graph.pending.is_some()),
        "the graph did not park on its pending source"
    );
    fixture.request = resume_request(first["cursor"].as_str().unwrap());
    fixture.owner.insert("padding", json!("p".repeat(padding)));
    fixture
}

#[test]
fn resumed_graph_cursor_admits_its_context_and_releases_its_sources_on_refusal() {
    let source = LedgerSource::new(&line("root"));
    let attachment = source.file("a.jsonl", &lines(0..64));
    let fits = |fixture: &Fixture| {
        traced(&fixture.store);
        settled_or_refused(&submit(fixture));
        reserved(&fixture.store).get(1) == Some(&value_bytes(&fixture.owner))
    };
    for background in [false, true] {
        let exact_for = |padding: usize| {
            let build = || pending_graph_fixture(&source, &attachment, background, padding);
            let sample = build();
            let key = sample
                .store
                .lock_state()
                .graphs
                .iter()
                .next()
                .map(|(token, _)| token.capacity())
                .expect("parked graph");
            let exact = exact_headroom(&build, &fits);
            assert_eq!(
                exact + key,
                REPLY_RESERVATION + value_bytes(&sample.owner),
                "the resumed graph did not admit its new context before cloning it"
            );
            exact
        };
        let (short, long) = (exact_for(0), exact_for(4096));
        assert_eq!(long - short, 4096);
        let build = || pending_graph_fixture(&source, &attachment, background, 4096);
        for headroom in [REPLY_RESERVATION, long - 1] {
            let refused = build();
            {
                let _filler = fill_to(&refused.store, &refused.owner, headroom);
                let root = Arc::downgrade(&refused.pins[0]);
                let pinned = Arc::strong_count(&refused.pins[0]);
                *refused.store.release_hook.lock().unwrap() = Some(Arc::new(
                    move |bytes: usize| {
                        assert_eq!(
                            root.strong_count() + 1,
                            pinned,
                            "a {bytes}-byte reservation was released while the refused graph cursor still held its root"
                        );
                    },
                ));
                assert!(!fits(&refused));
                *refused.store.release_hook.lock().unwrap() = None;
            }
            let control = build();
            {
                let mut state = control.store.lock_state();
                let token = state
                    .graphs
                    .iter()
                    .next()
                    .map(|(token, _)| token.clone())
                    .expect("parked graph");
                let mut graph = state.graphs.remove(&token).expect("parked graph");
                NativeStore::rollback_graph_page(&mut state, &mut graph);
            }
            evict_unpinned(&control.store, &control.owner);
            {
                let state = refused.store.lock_state();
                assert!(state.graphs.is_empty() && state.waiters.is_empty());
            }
            assert_eq!(
                (
                    settled(&refused.store),
                    audited(&refused.store),
                    bookkeeping(&refused.store)
                ),
                (
                    settled(&control.store),
                    audited(&control.store),
                    bookkeeping(&control.store)
                ),
                "a refused graph resume leaked its source waiter or leases"
            );
        }
        assert_fitted_at("resumed graph context", &build(), &fits, long);
    }
}

fn located(fixture: &Fixture) {
    let mut usage = [0u64; 18];
    match fixture.store.locate(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &mut usage,
    ) {
        Ok(_) => {}
        Err(error) if error.status == Status::RetainedLimit => {}
        Err(error) => panic!("location failed outside admission: {error:?}"),
    }
}

#[test]
fn fresh_location_cursor_admits_its_record_before_constructing_it() {
    let source = LedgerSource::new(&line("locate"));
    for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
        source.file(name, &line(name));
    }
    for background in [false, true] {
        let build = || location_park_fixture(&source, background);
        let control = build();
        assert!(location_parked(&control));
        let record = {
            let state = control.store.lock_state();
            let (_, cursor) = state.locates.iter().next().expect("parked location cursor");
            assert!(cursor.roots.is_empty() && cursor.found.is_empty());
            locate_record_bytes(cursor)
        };
        let site = reached(
            |store| store.locate_records.load(Ordering::Relaxed),
            located,
        );
        let exact = exact_headroom(&build, &site);
        assert_eq!(
            exact, record,
            "the location cursor's reservation is not its constructed record"
        );
        for headroom in [0, exact - 1] {
            let fixture = build();
            let _filler = fill_to(&fixture.store, &fixture.owner, headroom);
            let before = (
                ledger(&fixture.store),
                audited(&fixture.store),
                bookkeeping(&fixture.store),
            );
            traced(&fixture.store);
            let mut usage = [0u64; 18];
            let error = fixture
                .store
                .locate(
                    &fixture.request,
                    &fixture.owner,
                    &Cancellation::default(),
                    &mut usage,
                )
                .unwrap_err();
            assert_eq!(error.status, Status::RetainedLimit);
            assert_eq!(usage[17], 0, "the refused location examined entries");
            assert!(traced(&fixture.store).is_empty());
            assert_eq!(fixture.store.locate_records.load(Ordering::Relaxed), 0);
            assert!(fixture.store.lock_state().locates.is_empty());
            assert_eq!(
                (
                    ledger(&fixture.store),
                    audited(&fixture.store),
                    bookkeeping(&fixture.store)
                ),
                before
            );
        }
        refused_at_site("fresh location record", &build, &site, 0, exact);
    }
}

fn location_resume_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = cursor_store();
    let owner = context_for("locate-resume", background);
    let first = store.request(
        &json!({"schema":SCHEMA,"id":"ledger-locate","operation":"locate","session_ids":scenario.ids(),"roots":scenario.roots(),"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        &owner,
        &Cancellation::default(),
    );
    assert!(parked(&first));
    Fixture {
        store,
        owner,
        request: resume_request(first["cursor"].as_str().unwrap()),
        pins: Vec::new(),
    }
}

#[test]
fn resumed_location_step_admits_each_found_session_before_retaining_it() {
    let scenario = Scenario::new(9, |index| session_line(&format!("thread-{index:04}")));
    let root = PathBuf::from(&scenario.roots()[0]);
    for background in [false, true] {
        let build = || location_resume_fixture(&scenario, background);
        let control = build();
        let (key, mut found, pending) = {
            let state = control.store.lock_state();
            let (token, cursor) = state.locates.iter().next().expect("parked location cursor");
            assert!(cursor.pending.is_empty());
            (
                token.capacity(),
                cursor.found.clone(),
                cursor.pending.capacity(),
            )
        };
        let reply = submit(&control);
        let first = &reply["data"]["sessions"][0];
        let (id, path, revision) = (
            first["session_id"].as_str().unwrap().to_owned(),
            first["path"].as_str().unwrap().to_owned(),
            first["revision"].as_str().unwrap().to_owned(),
        );
        let found_before = found.capacity();
        found.insert(id.clone());
        let mut queued = VecDeque::<Value>::with_capacity(pending);
        queued.push_back(Value::new_null());
        let mut updates = Vec::<(String, PathBuf)>::new();
        updates.push(Default::default());
        let site = (found.capacity() - found_before) * size_of::<String>()
            + id.len()
            + (queued.capacity() - pending) * size_of::<Value>()
            + value_bytes(&json!({"session_id":id,"status":"ok","path":path,"revision":revision}))
            + updates.capacity() * size_of::<(String, PathBuf)>()
            + id.len()
            + root.join(Path::new(&path).file_name().unwrap()).capacity();
        let attempt = reached(
            |store| store.locate_items.load(Ordering::Relaxed),
            |fixture| settled_or_refused(&submit(fixture)),
        );
        let exact = exact_headroom(&build, &attempt);
        assert_eq!(
            exact + key,
            REPLY_RESERVATION + value_bytes(&control.owner) + LOCATE_PATH_SLOTS + site,
            "the first found session was not admitted by its retained bytes"
        );
        for headroom in [REPLY_RESERVATION, exact - 1] {
            let refused = build();
            let probe = refused.store.locate_items.load(Ordering::Relaxed);
            {
                let _filler = fill_to(&refused.store, &refused.owner, headroom);
                assert!(!attempt(&refused));
            }
            assert_eq!(refused.store.locate_items.load(Ordering::Relaxed), probe);
            let control = build();
            {
                let mut state = control.store.lock_state();
                let token = state
                    .locates
                    .iter()
                    .next()
                    .map(|(token, _)| token.clone())
                    .expect("parked location cursor");
                state.locates.remove(&token);
            }
            evict_unpinned(&control.store, &control.owner);
            assert!(refused.store.lock_state().locates.is_empty());
            assert_eq!(
                (
                    settled(&refused.store),
                    audited(&refused.store),
                    bookkeeping(&refused.store)
                ),
                (
                    settled(&control.store),
                    audited(&control.store),
                    bookkeeping(&control.store)
                ),
                "a refused location resume leaked state"
            );
        }
        assert_fitted_at("resumed found session", &build(), &attempt, exact);
    }
}

fn invalidated_location_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let fixture = location_resume_fixture(scenario, background);
    let token = fixture.request["cursor"].as_str().unwrap().to_owned();
    {
        let mut state = fixture.store.lock_state();
        assert_eq!(
            state.locates.pledged(&token),
            0,
            "the delivered location cursor kept its pledge"
        );
        let mut cursor = state
            .locates
            .remove(&token)
            .expect("parked location cursor");
        let forgotten = scenario
            .ids()
            .into_iter()
            .find(|id| !cursor.found.contains(id))
            .expect("an unfound session");
        cursor.pending.push_back(json!({"session_id":forgotten.as_str(),"status":"ok","path":scenario.root.directory.join("gone.jsonl").to_string_lossy().as_ref(),"revision":"gone"}));
        cursor.found.insert(forgotten);
        state.locates.reserve_for(&token);
        state.locates.insert(token.clone(), cursor);
    }
    fixture
}

#[test]
fn resumed_location_step_forgets_an_invalid_session_before_admitting_the_next_one() {
    let scenario = Scenario::new(9, |index| session_line(&format!("thread-{index:04}")));
    let root = PathBuf::from(&scenario.roots()[0]);
    for background in [false, true] {
        let build = || invalidated_location_fixture(&scenario, background);
        let control = build();
        let (key, found, forgotten, pending) = {
            let state = control.store.lock_state();
            let (token, cursor) = state.locates.iter().next().expect("parked location cursor");
            assert_eq!(cursor.pending.len(), 1);
            (
                token.capacity(),
                cursor.found.clone(),
                cursor.pending[0]["session_id"].as_str().unwrap().to_owned(),
                cursor.pending.capacity(),
            )
        };
        let reply = submit(&control);
        let first = &reply["data"]["sessions"][0];
        assert_eq!(first["status"].as_str(), Some("ok"), "{reply:?}");
        let (id, path, revision) = (
            first["session_id"].as_str().unwrap().to_owned(),
            first["path"].as_str().unwrap().to_owned(),
            first["revision"].as_str().unwrap().to_owned(),
        );
        let mut kept = HashSet::with_capacity(found.capacity());
        kept.extend(found.iter().filter(|member| **member != forgotten).cloned());
        assert_eq!(
            (kept.len(), kept.capacity()),
            (found.len() - 1, found.capacity())
        );
        let kept_before = kept.capacity();
        kept.insert(id.clone());
        let mut queued = VecDeque::<Value>::with_capacity(pending);
        queued.push_back(Value::new_null());
        let mut updates = Vec::<(String, PathBuf)>::new();
        updates.push(Default::default());
        let site = (kept.capacity() - kept_before) * size_of::<String>()
            + id.len()
            + (queued.capacity() - pending) * size_of::<Value>()
            + value_bytes(&json!({"session_id":id,"status":"ok","path":path,"revision":revision}))
            + updates.capacity() * size_of::<(String, PathBuf)>()
            + id.len()
            + root.join(Path::new(&path).file_name().unwrap()).capacity();
        let attempt = reached(
            |store| store.locate_items.load(Ordering::Relaxed),
            |fixture| settled_or_refused(&submit(fixture)),
        );
        let exact = exact_headroom(&build, &attempt);
        assert_eq!(
            exact + key,
            REPLY_RESERVATION
                + value_bytes(&control.owner)
                + LOCATE_PATH_SLOTS
                + found.len() * size_of::<String>()
                + site,
            "the session found after forgetting an invalid one was not admitted by its retained bytes"
        );
        for headroom in [REPLY_RESERVATION, exact - 1] {
            let refused = build();
            let probe = refused.store.locate_items.load(Ordering::Relaxed);
            {
                let _filler = fill_to(&refused.store, &refused.owner, headroom);
                assert!(!attempt(&refused));
            }
            assert_eq!(refused.store.locate_items.load(Ordering::Relaxed), probe);
            let control = build();
            {
                let mut state = control.store.lock_state();
                let token = state
                    .locates
                    .iter()
                    .next()
                    .map(|(token, _)| token.clone())
                    .expect("parked location cursor");
                state.locates.remove(&token);
            }
            evict_unpinned(&control.store, &control.owner);
            assert!(refused.store.lock_state().locates.is_empty());
            assert_eq!(
                (
                    settled(&refused.store),
                    audited(&refused.store),
                    bookkeeping(&refused.store)
                ),
                (
                    settled(&control.store),
                    audited(&control.store),
                    bookkeeping(&control.store)
                ),
                "a refused location resume leaked state after forgetting a session"
            );
        }
        assert_fitted_at(
            "resumed found session after an invalidation",
            &build(),
            &attempt,
            exact,
        );
    }
}

#[test]
fn filesystem_path_slot_covers_a_joined_directory_entry_path() {
    for root in (1..libc::PATH_MAX as usize).map(|length| "r".repeat(length)) {
        for name in [1, 255, 3 * 255].map(|length| "n".repeat(length)) {
            let joined = Path::new(&root).join(&name);
            assert!(
                joined.capacity() + size_of::<libc::dirent>() <= FILESYSTEM_PATH_BYTES,
                "a {}-byte root joined with a {}-byte name grew to {} bytes",
                root.len(),
                name.len(),
                joined.capacity()
            );
            assert!(
                joined.capacity() + libc::PATH_MAX as usize + 2 * size_of::<libc::dirent>()
                    <= LOCATE_PATH_SLOTS
            );
        }
    }
}

fn shared_root_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let store = prepared_store();
    let owner = context_for("shared-root", background);
    let (root, root_snapshot) = acquired(&store, &scenario.root.path, &owner);
    let direct = vec![scenario.root.path.to_string_lossy().into_owned()];
    let mut warming = warm_request(scenario);
    warming.insert("direct_paths", json!(direct));
    warm_with(&store, warming, scenario.sidechains.len() + 16, &owner);
    let request = prepare_request(&root, &scenario.ids(), &scenario.roots(), &direct);
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

#[test]
fn registered_prepare_graph_builds_a_shared_root_slice_in_one_admitted_allocation() {
    let scenario = Scenario::new(3, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let build = || shared_root_fixture(&scenario, background);
        let control = build();
        let response = submit(&control);
        assert!(admitted(&response, "ok"), "{response:?}");
        let graph_id = response["data"]["handle"]["graph_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let bytes = {
            let state = control.store.lock_state();
            let graph = state.prepared_graphs[&graph_id].lock().unwrap();
            let root = graph.root.stamp.identity.file();
            let membership = state
                .warm_memberships
                .values()
                .find(|membership| {
                    membership
                        .members
                        .iter()
                        .any(|member| member.stamp.identity.file() == root)
                })
                .expect("a warm membership sharing the root");
            assert!(!Arc::ptr_eq(&membership.members, &graph.sources));
            assert_eq!(graph.sources.len() + 1, membership.members.len());
            assert!(graph
                .sources
                .iter()
                .map(|source| &source.path)
                .eq(membership
                    .members
                    .iter()
                    .filter(|member| member.stamp.identity.file() != root)
                    .map(|member| &member.path)));
            constructed_graph_bytes(&state, &graph_id, &graph)
        };
        let site = reached(
            |store| store.registered_sources.load(Ordering::Relaxed),
            |fixture| settled_or_refused(&submit(fixture)),
        );
        let exact = exact_headroom(&build, &site);
        assert_eq!(
            exact,
            REPLY_RESERVATION + bytes,
            "the shared-root slice was built before its record was admitted"
        );
        refused_at_site(
            "registered shared root",
            &build,
            &site,
            REPLY_RESERVATION,
            exact,
        );
    }
}

struct ResumedArm {
    site: &'static str,
    extends_context: bool,
    slots: usize,
    grows: bool,
    reparks: bool,
    parked: fn(&StoreState) -> (String, usize, usize),
    len: fn(&StoreState) -> usize,
    table_bytes: fn(&StoreState) -> usize,
    remove: fn(&mut StoreState, &str),
}

fn admitted_bytes_of(trace: &[Trace]) -> Vec<usize> {
    trace
        .iter()
        .filter_map(|entry| match entry {
            Trace::Admitted(bytes) => Some(*bytes),
            _ => None,
        })
        .collect()
}

fn released_by(store: &NativeStore, run: impl FnOnce()) -> Vec<usize> {
    let released = Arc::new(Mutex::new(Vec::new()));
    *store.release_hook.lock().unwrap() = Some(Arc::new({
        let released = Arc::clone(&released);
        move |bytes: usize| released.lock().unwrap().push(bytes)
    }));
    run();
    *store.release_hook.lock().unwrap() = None;
    let released = released.lock().unwrap().clone();
    released
}

fn resumed(fixture: &Fixture) -> bool {
    match fixture.store.dispatch(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        &mut [0u64; 18],
    ) {
        Ok(_) => true,
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("resume failed outside admission: {error:?}"),
    }
}

fn held_by(arm: &ResumedArm, fixture: &Fixture) -> (usize, usize, usize) {
    let state = fixture.store.lock_state();
    assert_eq!((arm.len)(&state), 1, "{}: one cursor is parked", arm.site);
    let (token, walked, pledged) = (arm.parked)(&state);
    assert_eq!(
        pledged, 0,
        "{}: the delivered first page left its pledge on the parked cursor",
        arm.site
    );
    (
        token.capacity(),
        walked - token.capacity(),
        (arm.table_bytes)(&state),
    )
}

fn reparked_by(arm: &ResumedArm, fixture: &Fixture, key: usize, capacity: usize) -> usize {
    let state = fixture.store.lock_state();
    assert_eq!(
        (arm.len)(&state),
        usize::from(arm.reparks),
        "{}: the resumed page did not leave the cursor count it should",
        arm.site
    );
    if !arm.reparks {
        return 0;
    }
    let (token, walked, pledged) = (arm.parked)(&state);
    assert_eq!(token.capacity(), key);
    walked + (arm.table_bytes)(&state) - capacity + pledged
}

fn assert_resumed_arm_holds_its_charge(arm: &ResumedArm, build: &dyn Fn() -> Fixture) -> usize {
    let sample = build();
    let (key, held, capacity) = held_by(arm, &sample);
    let context = NativeStore::audit_value_bytes(&sample.owner);
    let extension = if arm.extends_context {
        context + arm.slots
    } else {
        0
    };
    if arm.extends_context {
        let fits = |fixture: &Fixture| {
            traced(&fixture.store);
            resumed(fixture);
            reserved(&fixture.store).first() == Some(&extension)
        };
        assert_eq!(
            exact_headroom(build, &fits) + key,
            extension,
            "{}: the resume freed more than its stored key before admitting its new context and path slots",
            arm.site
        );
    }
    let page = exact_headroom(build, &resumed);
    assert!(
        page > 0,
        "{}: the resumed page fit at zero headroom, so it has no refusal boundary to test",
        arm.site
    );
    for headroom in [0, page - 1] {
        let refused = build();
        {
            let _filler = fill_to(&refused.store, &refused.owner, headroom);
            assert!(
                !resumed(&refused),
                "{}: headroom {headroom} admitted the resumed page",
                arm.site
            );
        }
        refused.store.assert_conserved();
        let control = build();
        {
            let mut state = control.store.lock_state();
            let (token, _, _) = (arm.parked)(&state);
            (arm.remove)(&mut state, &token);
        }
        evict_unpinned(&control.store, &control.owner);
        assert_eq!(
            (arm.len)(&refused.store.lock_state()),
            0,
            "{}: a refused resume left its cursor parked",
            arm.site
        );
        assert_eq!(
            (
                settled(&refused.store),
                audited(&refused.store),
                bookkeeping(&refused.store)
            ),
            (
                settled(&control.store),
                audited(&control.store),
                bookkeeping(&control.store)
            ),
            "{}: a refused resume at headroom {headroom} did more than consume its parked cursor",
            arm.site
        );
        assert!(audited(&refused.store)[TOTAL] <= cap_for(&refused.owner));
    }
    let fitted = build();
    let leases_before = lease_walk(&fitted.store);
    let lease_capacity_before = fitted.store.lock_state().leases.capacity_bytes();
    let _filler = fill_to(&fitted.store, &fitted.owner, page);
    traced(&fitted.store);
    let released = released_by(&fitted.store, || {
        assert!(resumed(&fitted), "{}: the exact fit was refused", arm.site);
    });
    let trace = traced(&fitted.store);
    fitted.store.assert_conserved();
    let leases_after = lease_walk(&fitted.store);
    let lease_capacity_after = fitted.store.lock_state().leases.capacity_bytes();
    let leases = leases_after - leases_before + lease_capacity_after - lease_capacity_before;
    let additional = reparked_by(arm, &fitted, key, capacity);
    let reservations = reserved_traces(&trace);
    let fresh = reservations
        .iter()
        .skip(usize::from(arm.extends_context))
        .sum::<usize>();
    let covered = additional.min(held + extension + fresh);
    assert_eq!(
        page + key,
        extension + fresh + leases + additional - covered,
        "{}: the resumed page is not admitted at its new context, its fresh growth, its step's leases, and the part of its re-parked record that its held charge does not cover",
        arm.site
    );
    if arm.extends_context {
        assert_eq!(
            reservations.first(),
            Some(&extension),
            "{}: the step did not reserve its new context first: {trace:?}",
            arm.site
        );
    }
    assert_eq!(
        reservations.len() > usize::from(arm.extends_context),
        arm.grows,
        "{}: the step's fresh growth reservations do not match its arm: {trace:?}",
        arm.site
    );
    let admitted = admitted_bytes_of(&trace);
    if arm.reparks {
        assert_eq!(
            admitted.last(),
            Some(&(additional - covered)),
            "{}: the re-park admitted bytes its held charge already covered: {trace:?}",
            arm.site
        );
    }
    assert_eq!(
        admitted.iter().sum::<usize>(),
        extension + fresh + leases + additional - covered,
        "{}: the resumed page admitted bytes outside its context, fresh growth, leases, and re-park: {trace:?}",
        arm.site
    );
    assert_eq!(
        released,
        vec![held + extension + fresh - covered],
        "{}: the step released more or less than the uncovered remainder of its held charge",
        arm.site
    );
    assert!(audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner));
    page
}

fn parked_discovery(state: &StoreState) -> (String, usize, usize) {
    let (token, scan) = state.discoveries.iter().next().expect("parked discovery");
    (
        token.clone(),
        NativeStore::audit_discovery_bytes(token, scan),
        state.discoveries.pledged(token),
    )
}

fn discovery_resume_fixture(scenario: &Scenario, background: bool, padding: usize) -> Fixture {
    let mut fixture = discovery_fixture(scenario, background, padding);
    let first = submit(&fixture);
    assert!(parked(&first), "{first:?}");
    fixture.request = resume_request(first["cursor"].as_str().unwrap());
    fixture
}

#[test]
fn resumed_discovery_scan_holds_its_cursor_charge_through_the_page() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let arm = ResumedArm {
        site: "resumed discovery",
        extends_context: true,
        slots: LOCATE_PATH_SLOTS,
        grows: true,
        reparks: true,
        parked: parked_discovery,
        len: |state| state.discoveries.len(),
        table_bytes: |state| state.discoveries.capacity_bytes(),
        remove: |state, token| {
            state.discoveries.remove(token);
        },
    };
    for background in [false, true] {
        for padding in [0, 4096] {
            assert_resumed_arm_holds_its_charge(&arm, &|| {
                discovery_resume_fixture(&scenario, background, padding)
            });
        }
    }
}

fn parked_resolution(state: &StoreState) -> (String, usize, usize) {
    let (token, cursor) = state.resolutions.iter().next().expect("parked resolution");
    (
        token.clone(),
        NativeStore::audit_resolution_bytes(token, cursor),
        state.resolutions.pledged(token),
    )
}

fn resolution_resume_fixture(
    scenario: &Scenario,
    background: bool,
    padding: usize,
    ids: Vec<String>,
) -> Fixture {
    let store = fast_store();
    let mut owner = context_for("resolve-resume", background);
    owner.insert("padding", json!("p".repeat(padding)));
    let pins = scenario
        .sidechains
        .iter()
        .map(|path| {
            let (handle, snapshot) = acquired(&store, path, &owner);
            release_lease(&store, &handle, &owner);
            snapshot
        })
        .collect();
    let wanted = ids.len();
    let mut fixture = Fixture {
        store,
        request: json!({"schema":SCHEMA,"id":"ledger-resolve","operation":"resolve","session_ids":ids,"roots":scenario.roots(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        owner,
        pins,
    };
    let first = submit(&fixture);
    assert!(parked(&first), "{first:?}");
    {
        let state = fixture.store.lock_state();
        let (_, cursor) = state.resolutions.iter().next().expect("parked resolution");
        assert!(
            cursor.pending.is_none() && cursor.next == 1 && cursor.ids.len() == wanted,
            "the resolution did not park after its first cached session"
        );
    }
    fixture.request = resume_request(first["cursor"].as_str().unwrap());
    fixture
}

fn resolution_arm() -> ResumedArm {
    ResumedArm {
        site: "resumed resolution",
        extends_context: true,
        slots: 0,
        grows: true,
        reparks: false,
        parked: parked_resolution,
        len: |state| state.resolutions.len(),
        table_bytes: |state| state.resolutions.capacity_bytes(),
        remove: |state, token| {
            state.resolutions.remove(token);
        },
    }
}

#[test]
fn resumed_resolution_step_holds_its_cursor_charge_through_the_page() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    let arm = resolution_arm();
    for background in [false, true] {
        for padding in [0, 4096] {
            assert_resumed_arm_holds_its_charge(&arm, &|| {
                resolution_resume_fixture(
                    &scenario,
                    background,
                    padding,
                    vec![scenario.ids()[0].clone(), "absent".to_owned()],
                )
            });
        }
    }
}

#[test]
fn resumed_resolution_park_draws_its_record_from_the_held_charge() {
    let scenario = Scenario::new(3, |index| session_line(&format!("thread-{index:04}")));
    let arm = ResumedArm {
        reparks: true,
        ..resolution_arm()
    };
    for background in [false, true] {
        let fixture = resolution_resume_fixture(&scenario, background, 0, scenario.ids());
        let (key, held, capacity) = held_by(&arm, &fixture);
        let context = NativeStore::audit_value_bytes(&fixture.owner);
        let sessions_before = {
            let state = fixture.store.lock_state();
            let (_, cursor) = state.resolutions.iter().next().expect("parked resolution");
            cursor.sessions.capacity()
        };
        traced(&fixture.store);
        let released = released_by(&fixture.store, || {
            assert!(resumed(&fixture), "the resumed resolution was refused");
        });
        let trace = traced(&fixture.store);
        fixture.store.assert_conserved();
        let additional = reparked_by(&arm, &fixture, key, capacity);
        let fresh = {
            let state = fixture.store.lock_state();
            let (_, cursor) = state.resolutions.iter().next().expect("parked resolution");
            assert_eq!(cursor.sessions.len(), 2);
            (cursor.sessions.capacity() - sessions_before) * size_of::<Value>()
                + NativeStore::audit_value_bytes(&cursor.sessions[1])
        };
        let covered = additional.min(held + context + fresh);
        assert_eq!(
            reserved_traces(&trace).first(),
            Some(&context),
            "the resumed resolution did not admit its new context first: {trace:?}"
        );
        assert!(
            reserved_traces(&trace).contains(&fresh),
            "the resumed resolution did not reserve its resolved session before retaining it: {fresh} {trace:?}"
        );
        assert_eq!(
            admitted_bytes_of(&trace).last(),
            Some(&(additional - covered)),
            "the resolution park admitted bytes its held charge already covered: {trace:?}"
        );
        assert_eq!(
            released.last(),
            Some(&(held + context + fresh - covered)),
            "the resolution step released more or less than the uncovered remainder of its held charge: {released:?}"
        );
        assert!(audited(&fixture.store)[TOTAL] <= cap_for(&fixture.owner));
    }
}

fn parked_prepared_query(state: &StoreState) -> (String, usize, usize) {
    let (token, cursor) = state
        .prepared_queries
        .iter()
        .next()
        .expect("parked prepared query");
    (
        token.clone(),
        NativeStore::audit_prepared_query_bytes(token, cursor),
        state.prepared_queries.pledged(token),
    )
}

fn command_line(id: &str, bytes: usize) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":id,"sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":format!("{id}-call"),"name":"Bash","input":{"command":"x".repeat(bytes)}}]}})
    )
}

fn chunked_inputs_source() -> LedgerSource {
    LedgerSource::new(&format!(
        "{}{}",
        line("root"),
        (0..160)
            .map(|index| command_line(&format!("command-{index}"), 4096))
            .collect::<String>()
    ))
}

fn prepared_cursor_store() -> NativeStore {
    store_with(4096, 2048, &[("max_items_per_page", 1)])
}

fn chunked_query_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = prepared_cursor_store();
    let owner = context_for(&"q".repeat(300 * 1024), background);
    let (root, root_snapshot) = acquired(&store, &source.path, &owner);
    let graph = prepared_graph(&store, &root, &[], &owner);
    let mut fixture = Fixture {
        store,
        owner,
        request: graph_query(
            &graph,
            json!({"kind":"deep_predicate_inputs","order":"forward"}),
            json!([]),
        ),
        pins: vec![root_snapshot],
    };
    let first = submit(&fixture);
    assert!(parked(&first), "{first:?}");
    {
        let state = fixture.store.lock_state();
        let (_, cursor) = state
            .prepared_queries
            .iter()
            .next()
            .expect("parked prepared query");
        let records = cursor.input_records.as_ref().expect("queued input records");
        assert!(
            cursor.pending.is_none() && cursor.next == 0 && records.len() >= 2,
            "the prepared query did not park with two chunked records still queued"
        );
        assert!(
            records[0].capacity() < cursor.claimant.len(),
            "the {}-byte record the resume pops is not smaller than the {}-byte claimant its re-park pledges, so the resumed page needs no fresh bytes",
            records[0].capacity(),
            cursor.claimant.len()
        );
    }
    fixture.request = resume_request(first["cursor"].as_str().unwrap());
    fixture
}

#[test]
fn resumed_prepared_query_page_holds_its_cursor_charge_through_the_page() {
    let source = chunked_inputs_source();
    let arm = ResumedArm {
        site: "resumed prepared query",
        extends_context: false,
        slots: 0,
        grows: false,
        reparks: true,
        parked: parked_prepared_query,
        len: |state| state.prepared_queries.len(),
        table_bytes: |state| state.prepared_queries.capacity_bytes(),
        remove: |state, token| {
            state.prepared_queries.remove(token);
        },
    };
    for background in [false, true] {
        let build = || chunked_query_fixture(&source, background);
        let page = assert_resumed_arm_holds_its_charge(&arm, &build);
        let sample = build();
        let (delivered, popped) = {
            let state = sample.store.lock_state();
            let (token, cursor) = state
                .prepared_queries
                .iter()
                .next()
                .expect("parked prepared query");
            let delivered: Vec<usize> = state
                .deliveries
                .iter()
                .filter(|(_, delivery)| delivery.cursor.as_deref() == Some(token.as_str()))
                .map(|(key, delivery)| {
                    NativeStore::audit_delivery_bytes(key, delivery) + size_of::<(u64, Arc<str>)>()
                })
                .collect();
            assert_eq!(
                delivered.len(),
                1,
                "the first page was not delivered exactly once"
            );
            (
                delivered[0],
                cursor.input_records.as_ref().expect("queued input records")[0].capacity(),
            )
        };
        assert_eq!(
            page,
            delivered - popped,
            "the resumed page is not the delivery record its re-park pledges less the record it pops"
        );
    }
}

fn parked_projection(state: &StoreState) -> (String, usize, usize) {
    let (token, cursor) = state.projections.iter().next().expect("parked projection");
    (
        token.clone(),
        NativeStore::audit_projection_bytes(token, cursor),
        state.projections.pledged(token),
    )
}

fn projection_resume_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let mut fixture = projection_fixture(source, background, "projection-resume");
    let first = submit(&fixture);
    assert!(parked(&first), "{first:?}");
    fixture.request = resume_request(first["cursor"].as_str().unwrap());
    fixture
}

#[test]
fn resumed_projection_holds_its_cursor_charge_through_the_page() {
    let source = LedgerSource::new(&lines(0..3));
    let arm = ResumedArm {
        site: "resumed projection",
        extends_context: false,
        slots: 0,
        grows: false,
        reparks: true,
        parked: parked_projection,
        len: |state| state.projections.len(),
        table_bytes: |state| state.projections.capacity_bytes(),
        remove: |state, token| {
            state.projections.remove(token);
        },
    };
    for background in [false, true] {
        let build = || projection_resume_fixture(&source, background);
        assert_resumed_arm_holds_its_charge(&arm, &build);
        let fitted = build();
        let (key, held, capacity) = held_by(&arm, &fitted);
        assert!(resumed(&fitted));
        let reparked = reparked_by(&arm, &fitted, key, capacity);
        let pledged = {
            let state = fitted.store.lock_state();
            parked_projection(&state).2
        };
        assert_eq!(
            reparked,
            key + held + pledged,
            "the re-parked projection does not carry the resumed cursor's request and identity strings byte for byte"
        );
    }
}

fn discovery_record_bytes(fixture: &Fixture) -> usize {
    fixture.owner["claimant"].as_str().unwrap().len()
        + NativeStore::audit_value_bytes(&fixture.request)
        + NativeStore::audit_value_bytes(&fixture.owner)
        + fixture.request["roots"].as_array().unwrap().len() * size_of::<PathBuf>()
        + LOCATE_PATH_SLOTS
}

fn discovery_step_peak(token: &String, scan: &DiscoveryCursor, root: &Path) -> usize {
    NativeStore::audit_discovery_bytes(token, scan) - token.capacity()
        + LOCATE_PATH_SLOTS
        + std::fs::canonicalize(root).unwrap().as_os_str().len()
}

fn discovery_directory_reservation(scan: &DiscoveryCursor, root: &Path) -> usize {
    scan.seen_directories.capacity() * size_of::<SourceIdentity>()
        + scan.directories.capacity() * size_of::<OpenDirectory>()
        + arc_mirror::<(*mut libc::DIR, PathBuf)>()
        + std::fs::canonicalize(root).unwrap().as_os_str().len()
}

fn discovered(
    fixture: &Fixture,
    usage: &mut [u64; 18],
) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
    fixture.store.discover(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        usage,
    )
}

fn discovery_parked(fixture: &Fixture) -> bool {
    match discovered(fixture, &mut [0u64; 18]) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_some() && reason.is_some(),
                "the discovery page completed: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("discovery failed outside admission: {error:?}"),
    }
}

fn discovery_completed(fixture: &Fixture) -> bool {
    match discovered(fixture, &mut [0u64; 18]) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_none() && reason.is_none() && data["checkpoint"].as_str().is_some(),
                "the discovery did not complete into a checkpoint: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("discovery failed outside admission: {error:?}"),
    }
}

fn assert_record_refused(
    site: &str,
    fixture: &Fixture,
    headroom: usize,
    records: fn(&NativeStore) -> usize,
    parked: fn(&StoreState) -> usize,
    attempt: impl Fn(&Fixture, &mut [u64; 18]) -> Result<(), SnapshotError>,
) {
    let filler = fill_to(&fixture.store, &fixture.owner, headroom);
    let before = (
        ledger(&fixture.store),
        audited(&fixture.store),
        bookkeeping(&fixture.store),
    );
    traced(&fixture.store);
    let mut usage = [0u64; 18];
    let error = attempt(fixture, &mut usage).unwrap_err();
    assert_eq!(error.status, Status::RetainedLimit, "{site}");
    assert_eq!(usage[17], 0, "{site}: the refused record examined entries");
    assert!(traced(&fixture.store).is_empty(), "{site}");
    assert_eq!(records(&fixture.store), 0, "{site}: the record was built");
    assert_eq!(
        parked(&fixture.store.lock_state()),
        0,
        "{site}: a cursor was parked"
    );
    assert_eq!(
        (
            ledger(&fixture.store),
            audited(&fixture.store),
            bookkeeping(&fixture.store)
        ),
        before,
        "{site}: the refused record leaked state"
    );
    drop(filler);
    attempt(fixture, &mut [0u64; 18])
        .unwrap_or_else(|error| panic!("{site}: the retry after the refusal failed: {error:?}"));
    assert_eq!(
        records(&fixture.store),
        1,
        "{site}: the retry did not build the record"
    );
    fixture.store.assert_conserved();
}

fn first_directory_reservation(root: &Path, root_len: usize) -> usize {
    let mut seen: HashSet<SourceIdentity> = HashSet::new();
    seen.insert(SourceStamp::of(&std::fs::metadata(root).unwrap()).identity);
    let mut directories: Vec<[u8; size_of::<OpenDirectory>()]> = Vec::new();
    directories.push([0; size_of::<OpenDirectory>()]);
    seen.capacity() * size_of::<SourceIdentity>()
        + directories.capacity() * size_of::<OpenDirectory>()
        + arc_mirror::<(*mut libc::DIR, PathBuf)>()
        + root_len
}

fn assert_denied_directory_admission_opens_nothing(
    site: &str,
    build: &dyn Fn() -> Fixture,
    attempt: &dyn Fn(&Fixture) -> bool,
    directory: usize,
) {
    let opened = reached(
        |store| store.directory_opens.load(Ordering::Relaxed),
        |fixture| {
            attempt(fixture);
        },
    );
    let fit = exact_headroom(build, &opened);
    assert!(
        fit > 0,
        "{site}: the directory opened at zero headroom, so no admission was denied"
    );
    let fixture = build();
    {
        let _filler = fill_to(&fixture.store, &fixture.owner, fit - 1);
        let before = (
            ledger(&fixture.store),
            audited(&fixture.store),
            bookkeeping(&fixture.store),
        );
        traced(&fixture.store);
        assert!(
            !attempt(&fixture),
            "{site}: the denied directory admission was admitted"
        );
        let trace = traced(&fixture.store);
        assert_eq!(
            fixture.store.directory_opens.load(Ordering::Relaxed),
            0,
            "{site}: a denied directory admission opened the directory: {trace:?}"
        );
        assert!(
            !reserved_traces(&trace).contains(&directory),
            "{site}: the denied step reserved its directory anyway: {trace:?}"
        );
        assert!(
            !trace
                .iter()
                .any(|entry| matches!(entry, Trace::Allocated(_))),
            "{site}: the denied directory admission allocated retained bookkeeping: {trace:?}"
        );
        assert_eq!(
            (
                ledger(&fixture.store),
                audited(&fixture.store),
                bookkeeping(&fixture.store)
            ),
            before,
            "{site}: the denied directory admission leaked state"
        );
        fixture.store.assert_conserved();
    }
    traced(&fixture.store);
    assert!(
        attempt(&fixture),
        "{site}: the retry after the denied directory admission was refused"
    );
    assert_eq!(
        fixture.store.directory_opens.load(Ordering::Relaxed),
        1,
        "{site}: the retry did not open the directory exactly once"
    );
    assert!(
        reserved_traces(&traced(&fixture.store)).contains(&directory),
        "{site}: the retry did not reserve its directory before opening it"
    );
    fixture.store.assert_conserved();
}

#[test]
fn denied_discovery_directory_admission_opens_nothing_and_retries() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let root = &scenario.root.directory;
    let directory =
        first_directory_reservation(root, std::fs::canonicalize(root).unwrap().as_os_str().len());
    for background in [false, true] {
        assert_denied_directory_admission_opens_nothing(
            "fresh discovery directory",
            &|| discovery_fixture(&scenario, background, 0),
            &discovery_parked,
            directory,
        );
    }
}

#[test]
fn denied_location_directory_admission_opens_nothing_and_retries() {
    let source = LedgerSource::new(&line("locate"));
    for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
        source.file(name, &line(name));
    }
    let directory =
        first_directory_reservation(&source.directory, source.directory.as_os_str().len());
    for background in [false, true] {
        assert_denied_directory_admission_opens_nothing(
            "location directory",
            &|| location_park_fixture(&source, background),
            &location_parked,
            directory,
        );
    }
}

#[test]
fn denied_graph_listing_admission_opens_nothing_and_retries() {
    let source = subagent_graph_source();
    let directory = std::fs::canonicalize(&source.directory)
        .unwrap()
        .join("s")
        .join("subagents");
    let listed = 2 * directory.as_os_str().len() + arc_mirror::<(*mut libc::DIR, PathBuf)>();
    for background in [false, true] {
        assert_denied_directory_admission_opens_nothing(
            "graph listing",
            &|| graph_fixture(&source, background, json!([])),
            &|fixture: &Fixture| parked(&submit(fixture)),
            listed,
        );
    }
}

#[test]
fn fresh_discovery_admits_its_record_before_scanning() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    let root = &scenario.root.directory;
    for background in [false, true] {
        for padding in [0, 4096] {
            let build = || discovery_fixture(&scenario, background, padding);
            let record = discovery_record_bytes(&build());
            let site = reached(
                |store| store.discovery_records.load(Ordering::Relaxed),
                |fixture| {
                    discovery_parked(fixture);
                },
            );
            assert_eq!(
                exact_headroom(&build, &site),
                record,
                "the discovery cursor's reservation is not its constructed record"
            );
            for headroom in [0, record - 1] {
                assert_record_refused(
                    "fresh discovery record",
                    &build(),
                    headroom,
                    |store| store.discovery_records.load(Ordering::Relaxed),
                    |state| state.discoveries.len(),
                    |fixture, usage| discovered(fixture, usage).map(|_| ()),
                );
            }
            refused_at_site("fresh discovery record", &build, &site, 0, record);
            let step = exact_headroom(&build, &discovery_parked);
            refused_at_site("fresh discovery step", &build, &discovery_parked, 0, step);
            let fitted = build();
            let capacity = fitted.store.lock_state().discoveries.capacity_bytes();
            let _filler = fill_to(&fitted.store, &fitted.owner, step);
            traced(&fitted.store);
            let released = released_by(&fitted.store, || {
                assert!(discovery_parked(&fitted), "the exact fit was refused");
            });
            let trace = traced(&fitted.store);
            fitted.store.assert_conserved();
            assert_admitted_before_allocating("fresh discovery step", &trace);
            reserved_before_the_last_admission("fresh discovery step", &trace, record);
            let reservations = reserved_traces(&trace);
            assert_eq!(
                reservations.first(),
                Some(&record),
                "the discovery did not reserve its record first: {trace:?}"
            );
            let (peak, parked) = {
                let state = fitted.store.lock_state();
                let (token, scan) = state.discoveries.iter().next().expect("parked discovery");
                assert_eq!(token.capacity(), 64);
                assert!(
                    scan.roots.is_empty()
                        && scan.directories.len() == 1
                        && !scan.inventory.is_empty(),
                    "the parked discovery is not mid-directory"
                );
                assert!(
                    reservations.contains(&discovery_directory_reservation(scan, root)),
                    "the discovery step did not reserve its directory tables, root copy, and open-directory handle in one extension: {trace:?}"
                );
                (
                    discovery_step_peak(token, scan, root),
                    NativeStore::audit_discovery_bytes(token, scan)
                        + state.discoveries.capacity_bytes()
                        - capacity
                        + state.discoveries.pledged(token),
                )
            };
            assert_eq!(
                reservations.iter().sum::<usize>(),
                peak,
                "the discovery step's reservations are not its record, its path slots, and each retained growth: {trace:?}"
            );
            assert_eq!(
                step,
                peak.max(parked),
                "the discovery step's admission is not its peak reservation or its parked record"
            );
            assert_eq!(
                released,
                vec![step - parked],
                "the discovery park did not draw its record from the step's reservation"
            );
            let cap = cap_for(&fitted.owner);
            assert_eq!(ledger(&fitted.store)[TOTAL], cap - step + parked);
            assert_eq!(audited(&fitted.store)[TOTAL], cap - step + parked);
        }
    }
}

fn checkpointed_discovery_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let mut fixture = Fixture {
        store: fast_store(),
        owner: context_for("discover-checkpoint", background),
        request: json!({"schema":SCHEMA,"id":"ledger-discover","operation":"discover","roots":scenario.roots(),"checkpoint":null,"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        pins: Vec::new(),
    };
    let settled_reply = drive(&fixture.store, submit(&fixture), &fixture.owner);
    assert_eq!(
        settled_reply["status"].as_str(),
        Some("ok"),
        "{settled_reply:?}"
    );
    let checkpoint = settled_reply["data"]["checkpoint"]
        .as_str()
        .expect("discovery checkpoint")
        .to_owned();
    fixture.request.insert("checkpoint", json!(checkpoint));
    fixture
}

#[test]
fn checkpointed_discovery_admits_its_previous_inventory_before_cloning_it() {
    let scenario = Scenario::new(9, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let build = || checkpointed_discovery_fixture(&scenario, background);
        let control = build();
        let record = discovery_record_bytes(&control);
        let roots = NativeStore::audit_value_bytes(&control.request["roots"]);
        let previous = {
            let state = control.store.lock_state();
            assert_eq!(state.checkpoints.len(), 1);
            let (token, checkpoint) = state.checkpoints.iter().next().expect("checkpoint");
            assert_eq!(checkpoint.inventory.len(), 10);
            NativeStore::audit_checkpoint_bytes(token, checkpoint)
                - token.capacity()
                - checkpoint.claimant.capacity()
                - NativeStore::audit_value_bytes(&checkpoint.roots)
        };
        let exact = exact_headroom(&build, &discovery_completed);
        for headroom in [0, record - 1] {
            assert_record_refused(
                "checkpointed discovery record",
                &build(),
                headroom,
                |store| store.discovery_records.load(Ordering::Relaxed),
                |state| state.discoveries.len() + state.checkpoints.len() - 1,
                |fixture, usage| discovered(fixture, usage).map(|_| ()),
            );
        }
        refused_at_site(
            "checkpointed discovery",
            &build,
            &discovery_completed,
            0,
            exact,
        );
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let released = released_by(&fitted.store, || {
            assert!(discovery_completed(&fitted), "the exact fit was refused");
        });
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        let admitted_at = trace
            .iter()
            .position(|entry| matches!(entry, Trace::Admitted(_)))
            .expect("the rediscovery admitted nothing");
        assert!(
            trace
                .iter()
                .enumerate()
                .all(|(at, entry)| !matches!(entry, Trace::Allocated(_)) || at > admitted_at),
            "retained bookkeeping grew before its admission: {trace:?}"
        );
        reserved_before_the_last_admission("checkpointed discovery", &trace, record);
        let reservations = reserved_traces(&trace);
        assert_eq!(
            reservations.first(),
            Some(&record),
            "the rediscovery did not reserve its record first: {trace:?}"
        );
        assert!(
            reservations.contains(&previous),
            "the rediscovery did not reserve its previous inventory before cloning it: {previous} {trace:?}"
        );
        assert!(
            reservations.contains(&roots),
            "the checkpoint did not reserve its roots before cloning them: {roots} {trace:?}"
        );
        let peak = reservations.iter().sum::<usize>();
        let retained = ledger(&fitted.store)[TOTAL] + exact - cap;
        assert_eq!(fitted.store.lock_state().checkpoints.len(), 2);
        assert_eq!(
            exact,
            peak.max(retained),
            "the rediscovery's admission is not its peak reservation or its new checkpoint"
        );
        assert_eq!(
            admitted_bytes_of(&trace).last(),
            Some(&(retained - retained.min(peak))),
            "the checkpoint admitted bytes the scan's reservation already covered: {trace:?}"
        );
        assert_eq!(
            released,
            vec![exact - retained],
            "the checkpoint did not draw its record from the scan's reservation"
        );
        assert_eq!(audited(&fitted.store)[TOTAL], cap - exact + retained);
    }
}

fn resolution_record_bytes(fixture: &Fixture) -> usize {
    let ids = fixture.request["session_ids"].as_array().unwrap();
    fixture.owner["claimant"].as_str().unwrap().len()
        + NativeStore::audit_value_bytes(&fixture.owner)
        + NativeStore::audit_value_bytes(&fixture.request)
        + ids.len() * size_of::<String>()
        + ids
            .iter()
            .map(|id| id.as_str().unwrap().len())
            .sum::<usize>()
}

fn resolved(
    fixture: &Fixture,
    usage: &mut [u64; 18],
) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
    fixture.store.resolve(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        usage,
    )
}

fn resolution_completed(fixture: &Fixture) -> bool {
    match resolved(fixture, &mut [0u64; 18]) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_none() && reason.is_none(),
                "the resolution parked: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("resolution failed outside admission: {error:?}"),
    }
}

fn resolution_parked(fixture: &Fixture) -> bool {
    match resolved(fixture, &mut [0u64; 18]) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_some() && reason.is_some(),
                "the resolution completed: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("resolution failed outside admission: {error:?}"),
    }
}

fn fresh_resolution_fixture(
    scenario: &Scenario,
    background: bool,
    ids: Vec<String>,
    cached: bool,
) -> Fixture {
    let store = fast_store();
    let owner = context_for("resolve-fresh", background);
    let pins = if cached {
        scenario
            .sidechains
            .iter()
            .map(|path| {
                let (handle, snapshot) = acquired(&store, path, &owner);
                release_lease(&store, &handle, &owner);
                snapshot
            })
            .collect()
    } else {
        Vec::new()
    };
    Fixture {
        store,
        request: json!({"schema":SCHEMA,"id":"ledger-resolve","operation":"resolve","session_ids":ids,"roots":scenario.roots(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+120_000,"limits":limits_json()}),
        owner,
        pins,
    }
}

#[test]
fn fresh_resolution_admits_its_record_before_walking() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    let ids = vec!["absent-a".to_owned(), "absent-bb".to_owned()];
    for background in [false, true] {
        let build = || fresh_resolution_fixture(&scenario, background, ids.clone(), false);
        let record = resolution_record_bytes(&build());
        let site = reached(
            |store| store.resolution_records.load(Ordering::Relaxed),
            |fixture| {
                resolution_completed(fixture);
            },
        );
        assert_eq!(
            exact_headroom(&build, &site),
            record,
            "the resolution cursor's reservation is not its constructed record"
        );
        for headroom in [0, record - 1] {
            assert_record_refused(
                "fresh resolution record",
                &build(),
                headroom,
                |store| store.resolution_records.load(Ordering::Relaxed),
                |state| state.resolutions.len(),
                |fixture, usage| resolved(fixture, usage).map(|_| ()),
            );
        }
        refused_at_site("fresh resolution record", &build, &site, 0, record);
        let mut sessions: Vec<Value> = Vec::new();
        let mut growth = Vec::new();
        for id in &ids {
            let capacity = sessions.capacity();
            sessions.push(json!({"session_id":id,"status":"missing","description":null}));
            growth.push(
                (sessions.capacity() - capacity) * size_of::<Value>()
                    + NativeStore::audit_value_bytes(sessions.last().unwrap()),
            );
        }
        let exact = exact_headroom(&build, &resolution_completed);
        assert_eq!(
            exact,
            record + growth.iter().sum::<usize>(),
            "the completing resolution is not admitted at its record and each session it retains"
        );
        refused_at_site("fresh resolution", &build, &resolution_completed, 0, exact);
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let released = released_by(&fitted.store, || {
            assert!(resolution_completed(&fitted), "the exact fit was refused");
        });
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        assert_eq!(
            reserved_traces(&trace),
            [vec![record], growth].concat(),
            "the resolution reserved something besides its record and sessions: {trace:?}"
        );
        assert_eq!(admitted_bytes_of(&trace).iter().sum::<usize>(), exact);
        assert!(
            !trace
                .iter()
                .any(|entry| matches!(entry, Trace::Allocated(_))),
            "the completing resolution allocated retained bookkeeping: {trace:?}"
        );
        assert_eq!(
            released,
            vec![exact],
            "the completing resolution did not release its whole reservation"
        );
        assert!(fitted.store.lock_state().resolutions.is_empty());
        assert_eq!(ledger(&fitted.store)[TOTAL], cap - exact);
        assert_eq!(audited(&fitted.store)[TOTAL], cap - exact);
    }
}

#[test]
fn fresh_resolution_reserves_each_found_session_before_retaining_it() {
    let scenario = Scenario::new(2, |index| session_line(&format!("thread-{index:04}")));
    let root = PathBuf::from(&scenario.roots()[0]);
    for background in [false, true] {
        let build = || fresh_resolution_fixture(&scenario, background, scenario.ids(), true);
        let record = resolution_record_bytes(&build());
        let exact = exact_headroom(&build, &resolution_parked);
        refused_at_site(
            "fresh resolution step",
            &build,
            &resolution_parked,
            0,
            exact,
        );
        let fitted = build();
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        assert!(resolution_parked(&fitted), "the exact fit was refused");
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        assert_admitted_before_allocating("fresh resolution step", &trace);
        reserved_before_the_last_admission("fresh resolution step", &trace, record);
        let reservations = reserved_traces(&trace);
        assert_eq!(
            reservations.first(),
            Some(&record),
            "the resolution did not reserve its record first: {trace:?}"
        );
        let mut found: HashMap<String, PathBuf> = HashMap::new();
        for id in scenario.ids() {
            let path = root.join(format!("{id}.jsonl"));
            let bytes = id.len() + path.capacity();
            let capacity = found.capacity();
            found.insert(id, path);
            let growth = (found.capacity() - capacity) * size_of::<(String, PathBuf)>() + bytes;
            assert!(
                reservations.contains(&growth),
                "the walk did not reserve a found session's table growth, id, and path before retaining it: {growth} {trace:?}"
            );
        }
        let session = {
            let state = fitted.store.lock_state();
            let (_, cursor) = state.resolutions.iter().next().expect("parked resolution");
            assert_eq!(cursor.sessions.len(), 1);
            let mut mirror: Vec<Value> = Vec::new();
            mirror.push(Value::new_null());
            mirror.capacity() * size_of::<Value>()
                + NativeStore::audit_value_bytes(&cursor.sessions[0])
        };
        assert!(
            reservations.contains(&session),
            "the step did not reserve its resolved session before retaining it: {session} {trace:?}"
        );
        assert!(audited(&fitted.store)[TOTAL] <= cap_for(&fitted.owner));
    }
}

fn prepared_query_record_bytes(fixture: &Fixture) -> usize {
    size_of::<PreparedQueryCursor>()
        + fixture.owner["claimant"].as_str().unwrap().len()
        + fixture.request["handle"]["graph_id"]
            .as_str()
            .unwrap()
            .len()
        + NativeStore::audit_value_bytes(&fixture.request["query"])
}

fn predicate_queue_bound(fixture: &Fixture) -> (usize, usize) {
    let graph_id = fixture.request["handle"]["graph_id"].as_str().unwrap();
    let encoded = {
        let state = fixture.store.lock_state();
        let graph = state.prepared_graphs[graph_id].lock().unwrap();
        sonic_rs::to_vec(&graph.root_facts.inputs).unwrap().len()
    };
    let records = 2 * (4 * encoded) / crate::snapshot_codec::PREDICATE_INPUT_CHUNK_BYTES + 2;
    let framing = r#"{"calls":[],"commands":[],"edited_files":[],"skills":[]}"#.len();
    (records, records * (size_of::<String>() + framing) + encoded)
}

fn fresh_query_fixture(source: &LedgerSource, background: bool) -> Fixture {
    let store = prepared_cursor_store();
    let owner = context_for("query-fresh", background);
    let (root, root_snapshot) = acquired(&store, &source.path, &owner);
    let graph = prepared_graph(&store, &root, &[], &owner);
    Fixture {
        store,
        owner,
        request: graph_query(
            &graph,
            json!({"kind":"deep_predicate_inputs","order":"forward"}),
            json!([]),
        ),
        pins: vec![root_snapshot],
    }
}

fn queried(
    fixture: &Fixture,
    usage: &mut [u64; 18],
) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
    fixture.store.query_graph(
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        usage,
    )
}

fn query_parked(fixture: &Fixture) -> bool {
    match queried(fixture, &mut [0u64; 18]) {
        Ok((data, cursor, reason)) => {
            assert!(
                cursor.is_some() && reason.is_some(),
                "the prepared query completed: {data:?}"
            );
            true
        }
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("prepared query failed outside admission: {error:?}"),
    }
}

#[test]
fn fresh_prepared_query_admits_its_cursor_before_building_its_record_queue() {
    let source = chunked_inputs_source();
    for background in [false, true] {
        let build = || fresh_query_fixture(&source, background);
        let control = build();
        let record = prepared_query_record_bytes(&control);
        let (records, queue) = predicate_queue_bound(&control);
        let site = reached(
            |store| store.prepared_query_records.load(Ordering::Relaxed),
            |fixture| {
                query_parked(fixture);
            },
        );
        assert_eq!(
            exact_headroom(&build, &site),
            record,
            "the prepared query cursor's reservation is not its constructed record"
        );
        for headroom in [0, record - 1] {
            assert_record_refused(
                "fresh prepared query record",
                &build(),
                headroom,
                |store| store.prepared_query_records.load(Ordering::Relaxed),
                |state| state.prepared_queries.len(),
                |fixture, usage| queried(fixture, usage).map(|_| ()),
            );
        }
        refused_at_site("fresh prepared query record", &build, &site, 0, record);
        let exact = exact_headroom(&build, &query_parked);
        refused_at_site("fresh prepared query", &build, &query_parked, 0, exact);
        let fitted = build();
        let capacity = fitted.store.lock_state().prepared_queries.capacity_bytes();
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let released = released_by(&fitted.store, || {
            assert!(query_parked(&fitted), "the exact fit was refused");
        });
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        assert_admitted_before_allocating("fresh prepared query", &trace);
        reserved_before_the_last_admission("fresh prepared query", &trace, record);
        assert_eq!(
            reserved_traces(&trace),
            vec![record, queue],
            "the prepared query reserved something besides its cursor record and its record-queue bound: {trace:?}"
        );
        let parked = {
            let state = fitted.store.lock_state();
            let (token, cursor) = state
                .prepared_queries
                .iter()
                .next()
                .expect("parked prepared query");
            let queued = cursor.input_records.as_ref().expect("input records");
            assert_eq!(
                queued.capacity(),
                records,
                "the record queue was not built at its record bound"
            );
            assert!(queued.len() >= 2, "the page drained the chunked queue");
            assert!(
                queued.capacity() * size_of::<String>()
                    + queued.iter().map(String::capacity).sum::<usize>()
                    <= queue,
                "the record queue outgrew its byte bound"
            );
            NativeStore::audit_prepared_query_bytes(token, cursor)
                + state.prepared_queries.capacity_bytes()
                - capacity
                + state.prepared_queries.pledged(token)
        };
        assert_eq!(
            exact,
            (record + queue).max(parked),
            "the prepared query's admission is not its peak reservation or its parked record"
        );
        assert_eq!(
            released,
            vec![exact - parked],
            "the prepared query park did not draw its record from the reservation"
        );
        let cap = cap_for(&fitted.owner);
        assert_eq!(ledger(&fitted.store)[TOTAL], cap - exact + parked);
        assert_eq!(audited(&fitted.store)[TOTAL], cap - exact + parked);
    }
}

fn membership_fixture(scenario: &Scenario, background: bool) -> Fixture {
    let mut request = warm_request(scenario);
    request.insert("thread_ids", json!([]));
    request.insert("direct_paths", json!(scenario.direct()));
    Fixture {
        store: prepared_store(),
        owner: context_for("warm-membership", background),
        request,
        pins: Vec::new(),
    }
}

fn membership_key(fixture: &Fixture) -> String {
    NativeStore::warm_membership_key(&fixture.request, &fixture.owner).unwrap()
}

fn membership_record_bytes(fixture: &Fixture) -> usize {
    membership_key(fixture).len()
        + size_of::<WarmMembership>()
        + 2 * <Sha256 as Digest>::output_size()
}

fn built_membership(
    fixture: &Fixture,
    usage: &mut [u64; 18],
) -> Result<(WarmMembership, ProjectionReservation<'_>), SnapshotError> {
    let mut remaining = work_bounds();
    fixture.store.build_warm_membership(
        &membership_key(fixture),
        &fixture.request,
        &fixture.owner,
        &Cancellation::default(),
        usage,
        &mut remaining,
    )
}

fn membership_built(fixture: &Fixture) -> bool {
    match built_membership(fixture, &mut [0u64; 18]) {
        Ok(_) => true,
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("membership build failed outside admission: {error:?}"),
    }
}

fn membership_published(fixture: &Fixture) -> bool {
    let (membership, mut reservation) = match built_membership(fixture, &mut [0u64; 18]) {
        Ok(built) => built,
        Err(error) if error.status == Status::RetainedLimit => return false,
        Err(error) => panic!("membership build failed outside admission: {error:?}"),
    };
    match fixture.store.publish_warm_membership(
        &membership_key(fixture),
        &membership,
        &fixture.owner,
        Some(&mut reservation),
    ) {
        Ok(()) => true,
        Err(error) if error.status == Status::RetainedLimit => false,
        Err(error) => panic!("membership publication failed outside admission: {error:?}"),
    }
}

#[test]
fn fresh_warm_membership_admits_its_record_before_building_its_buffers() {
    let scenario = Scenario::new(4, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let build = || membership_fixture(&scenario, background);
        let record = membership_record_bytes(&build());
        let site = reached(
            |store| store.warm_records.load(Ordering::Relaxed),
            |fixture| {
                membership_built(fixture);
            },
        );
        assert_eq!(
            exact_headroom(&build, &site),
            record,
            "the warm membership's reservation is not its key, record, and revision"
        );
        for headroom in [0, record - 1] {
            assert_record_refused(
                "fresh warm membership record",
                &build(),
                headroom,
                |store| store.warm_records.load(Ordering::Relaxed),
                |state| state.warm_memberships.len(),
                |fixture, usage| built_membership(fixture, usage).map(|_| ()),
            );
        }
        refused_at_site("fresh warm membership record", &build, &site, 0, record);
        let exact = exact_headroom(&build, &membership_built);
        refused_at_site("fresh warm membership", &build, &membership_built, 0, exact);
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let (membership, reservation) =
            built_membership(&fitted, &mut [0u64; 18]).expect("the exact fit was refused");
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        let reservations = reserved_traces(&trace);
        assert_eq!(
            reservations.first(),
            Some(&record),
            "the membership did not reserve its record first: {trace:?}"
        );
        assert_eq!(reservations.iter().sum::<usize>(), exact);
        assert_eq!(
            reservation.bytes, exact,
            "the build does not hold every byte it reserved"
        );
        assert_eq!(admitted_bytes_of(&trace).iter().sum::<usize>(), exact);
        assert!(
            !trace
                .iter()
                .any(|entry| matches!(entry, Trace::Allocated(_))),
            "the membership build allocated retained bookkeeping: {trace:?}"
        );
        assert_eq!(
            (membership.members.len(), membership.sidechain_dirs.len()),
            (4, 4)
        );
        let mut members: Vec<PreparedSourceRef> = Vec::new();
        let mut dirs: Vec<(PathBuf, Option<SourceStamp>)> = Vec::new();
        let mut growth = record;
        for member in membership.members.iter() {
            let capacity = members.capacity();
            members.push(member.clone());
            growth += (members.capacity() - capacity) * size_of::<PreparedSourceRef>()
                + member.path.capacity();
        }
        for dir in membership.sidechain_dirs.iter() {
            let capacity = dirs.capacity();
            dirs.push(dir.clone());
            growth += (dirs.capacity() - capacity) * size_of::<(PathBuf, Option<SourceStamp>)>()
                + dir.0.capacity();
        }
        growth += arc_slice_mirror::<PreparedSourceRef>(membership.members.len())
            + arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(membership.sidechain_dirs.len());
        assert_eq!(
            exact, growth,
            "the membership build is not admitted at its record, each retained path and buffer growth, and its shared slices"
        );
        assert_eq!(ledger(&fitted.store)[TOTAL], cap);
        let released = released_by(&fitted.store, || drop(reservation));
        assert_eq!(released, vec![exact]);
        assert_eq!(ledger(&fitted.store)[TOTAL], cap - exact);
        drop(membership);
    }
}

#[test]
fn published_warm_membership_draws_its_record_from_the_build_reservation() {
    let scenario = Scenario::new(4, |index| line(&format!("thread-{index:04}")));
    for background in [false, true] {
        let build = || membership_fixture(&scenario, background);
        let record = membership_record_bytes(&build());
        let peak = exact_headroom(&build, &membership_built);
        let exact = exact_headroom(&build, &membership_published);
        refused_at_site(
            "warm membership publication",
            &build,
            &membership_published,
            0,
            exact,
        );
        let fitted = build();
        let cap = cap_for(&fitted.owner);
        let shared_table = fitted.store.lock_state().ledger.shared.table_bytes();
        let _filler = fill_to(&fitted.store, &fitted.owner, exact);
        traced(&fitted.store);
        let released = released_by(&fitted.store, || {
            assert!(membership_published(&fitted), "the exact fit was refused");
        });
        let trace = traced(&fitted.store);
        fitted.store.assert_conserved();
        assert_admitted_before_allocating("warm membership publication", &trace);
        reserved_before_the_last_admission("warm membership publication", &trace, record);
        let retained = ledger(&fitted.store)[TOTAL] + exact - cap;
        let stored = {
            let state = fitted.store.lock_state();
            assert_eq!(state.warm_memberships.len(), 1);
            let (key, membership) = state.warm_memberships.iter().next().unwrap();
            NativeStore::audit_warm_membership_bytes(key, membership)
                + state.warm_memberships.capacity_bytes()
                + NativeStore::audit_warm_buffer_bytes(&state)
                + state.ledger.shared.table_bytes()
                - shared_table
        };
        assert_eq!(
            retained, stored,
            "the published membership retains more than its record, its table growth, and its shared buffers"
        );
        assert_eq!(
            exact,
            peak.max(retained),
            "the publication's admission is not the build's peak reservation or its retained record"
        );
        assert_eq!(
            admitted_bytes_of(&trace).last(),
            Some(&(retained - retained.min(peak))),
            "the publication admitted bytes the build's reservation already covered: {trace:?}"
        );
        assert_eq!(
            released,
            vec![exact - retained],
            "the publication did not draw its record from the build's reservation"
        );
        assert_eq!(audited(&fitted.store)[TOTAL], cap - exact + retained);
    }
}

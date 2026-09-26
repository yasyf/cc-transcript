use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::gateway::{parse_transcript_bytes, sniff_provider, Provider};
use crate::snapshot_activity::ActivityIndex;
use crate::snapshot_memory::{entry_charge, MemoryCharge};
use crate::types::Entry;

pub const SCHEMA: &str = "cc-transcript.snapshot/1";
pub const PARSER_VERSION: &str = "cc-transcript.snapshot/1";
pub const MAX_REPLY_BYTES: usize = 1_044_480;
const MAX_DATA_BYTES: usize = MAX_REPLY_BYTES - 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Incomplete,
    Missing,
    Changed,
    SourceLimit,
    EntryLimit,
    RetainedLimit,
    LeaseLimit,
    OutputLimit,
    Deadline,
    Cancelled,
    ParseError,
    PermissionDenied,
    StaleHandle,
    StaleCursor,
    InvalidRequest,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::Missing => "missing",
            Self::Changed => "changed",
            Self::SourceLimit => "source_limit",
            Self::EntryLimit => "entry_limit",
            Self::RetainedLimit => "retained_limit",
            Self::LeaseLimit => "lease_limit",
            Self::OutputLimit => "output_limit",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::ParseError => "parse_error",
            Self::PermissionDenied => "permission_denied",
            Self::StaleHandle => "stale_handle",
            Self::StaleCursor => "stale_cursor",
            Self::InvalidRequest => "invalid_request",
        }
    }
}

#[derive(Debug)]
pub struct SnapshotError {
    pub status: Status,
    pub reason: String,
}

impl SnapshotError {
    pub fn new(status: Status, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WorkLimits {
    pub max_read_bytes: usize,
    pub max_events: usize,
    pub max_items: usize,
    pub max_output_bytes: usize,
    pub max_discovery_entries: usize,
    pub max_sources: usize,
    pub deadline_unix_ms: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn check(&self, deadline_unix_ms: u64) -> Result<(), SnapshotError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(SnapshotError::new(Status::Cancelled, "request cancelled"));
        }
        if now_ms() >= deadline_unix_ms {
            return Err(SnapshotError::new(
                Status::Deadline,
                "request deadline expired",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct SourceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStamp {
    pub identity: SourceIdentity,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl SourceStamp {
    pub fn of(metadata: &Metadata) -> Self {
        Self {
            identity: SourceIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            size: metadata.len(),
            mtime_ns: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            ctime_ns: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
        }
    }

    fn revision(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.identity.device, self.identity.inode, self.size, self.mtime_ns, self.ctime_ns
        )
    }
}

#[derive(Debug)]
pub struct EntryChunk {
    pub entries: Arc<Vec<Entry>>,
    pub start: usize,
    pub charge: MemoryCharge,
    pub entry_charges: Vec<MemoryCharge>,
    pub user_count: usize,
    pub sidechain_user_count: usize,
}

impl EntryChunk {
    pub fn new(start: usize, entries: Vec<Entry>) -> Self {
        let mut charge = MemoryCharge {
            owned_capacity_bytes: size_of::<Self>() + entries.capacity() * size_of::<Entry>(),
            opaque_dom_accounted_bytes: 0,
        };
        let entry_charges: Vec<_> = entries.iter().map(entry_charge).collect();
        for entry_charge in &entry_charges {
            charge += *entry_charge;
        }
        charge.owned_capacity_bytes += entry_charges.capacity() * size_of::<MemoryCharge>();
        let user_count = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::User(_)))
            .count();
        let sidechain_user_count = entries
            .iter()
            .filter(|entry| matches!(entry,Entry::User(user) if user.meta.is_sidechain))
            .count();
        Self {
            entries: Arc::new(entries),
            start,
            charge,
            entry_charges,
            user_count,
            sidechain_user_count,
        }
    }
}

#[derive(Debug)]
pub struct TranscriptSnapshot {
    pub id: String,
    pub canonical_path: PathBuf,
    pub stamp: SourceStamp,
    pub provider: Provider,
    pub session_id: String,
    pub chunks: Vec<Arc<EntryChunk>>,
    pub activity: Arc<ActivityIndex>,
    pub committed_bytes: u64,
    pub provisional_tail: bool,
    pub fence: Vec<u8>,
    pub event_count: usize,
}

impl TranscriptSnapshot {
    pub fn entries(&self) -> Vec<&Entry> {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.entries.iter())
            .collect()
    }

    pub fn entry(&self, position: usize) -> &Entry {
        let at = self.chunks.partition_point(|chunk| chunk.start <= position) - 1;
        &self.chunks[at].entries[position - self.chunks[at].start]
    }

    pub fn range(&self, range: Range<usize>) -> Vec<&Entry> {
        range.map(|index| self.entry(index)).collect()
    }

    pub fn accounted_allocations(&self) -> Vec<(usize, MemoryCharge)> {
        let mut entries = vec![(
            self as *const Self as usize,
            MemoryCharge {
                owned_capacity_bytes: size_of::<Self>()
                    + self.id.capacity()
                    + self.canonical_path.as_os_str().len()
                    + self.session_id.capacity()
                    + self.chunks.capacity() * size_of::<Arc<EntryChunk>>()
                    + self.fence.capacity(),
                opaque_dom_accounted_bytes: 0,
            },
        )];
        entries.extend(
            self.chunks
                .iter()
                .map(|chunk| (Arc::as_ptr(&chunk.entries) as usize, chunk.charge)),
        );
        entries
    }
}

pub struct Projection {
    pub read_bytes: usize,
    pub events: usize,
    pub items: usize,
    pub data: Value,
    pub complete: bool,
    pub next: Option<usize>,
    pub reason: Option<String>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as u64
}

use sha2::{Digest, Sha256};
use sonic_rs::json;

const COUNTERS: [&str; 18] = [
    "source_opens",
    "source_bytes_read",
    "bytes_decoded",
    "events_parsed",
    "cold_parses",
    "append_parses",
    "activity_lifts",
    "cache_hits",
    "inflight_joins",
    "generations_published",
    "generations_invalidated",
    "requests_cancelled",
    "requests_failed",
    "output_bytes",
    "transport_bytes",
    "nonincremental_lowering_calls",
    "nonincremental_lowering_source_bytes",
    "discovery_entries_examined",
];

#[derive(Clone)]
struct Config {
    retained: usize,
    source: usize,
    entry: usize,
    output: usize,
    leases: usize,
    ttl: u64,
    preparation: u64,
    loads: usize,
    read_step: usize,
    event_step: usize,
    page_items: usize,
    hook_loads: usize,
    hook_leases: usize,
    hook_bytes: usize,
}

struct Lease {
    claimant: String,
    snapshot: Arc<TranscriptSnapshot>,
    classifier: Value,
    expires: u64,
    absolute_deadline: u64,
    exposed: bool,
    registry_generation: String,
    registry: Arc<crate::toolcall::ToolRegistrySnapshot>,
}

#[derive(Clone)]
struct Waiter {
    claimant: String,
    load: Arc<LoadSlot>,
    classifier: Value,
    context: Value,
    limits: WorkLimits,
    created: u64,
    expires: u64,
    deadline: u64,
    used_bytes: usize,
    used_events: usize,
    busy: bool,
}

struct LoadSlot {
    id: String,
    path: PathBuf,
    stamp: SourceStamp,
    registry_generation: String,
    registry: Arc<crate::toolcall::ToolRegistrySnapshot>,
    work: Mutex<Load>,
    accounted: AtomicUsize,
    deadline: u64,
}

struct Load {
    file: File,
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    pending_start: u64,
    provider: Option<Provider>,
    chunks: Vec<Arc<EntryChunk>>,
    count: usize,
    activity: ActivityIndex,
    indexed: usize,
    decoded: bool,
    session_id: Option<String>,
    origin_fence: Vec<u8>,
    origin_complete: bool,
    seal_fence: Vec<u8>,
    sealed: bool,
    prefix_fence: Vec<u8>,
    previous: Option<Arc<TranscriptSnapshot>>,
    previous_index_compatible: bool,
    prefix_checked: bool,
    fence: Vec<u8>,
    committed: u64,
    provisional: bool,
    result: Option<Arc<TranscriptSnapshot>>,
    failure: Option<SnapshotError>,
}

#[derive(Clone)]
struct ProjectionCursor {
    claimant: String,
    registry_generation: String,
    admission: String,
    request: Value,
    limits: WorkLimits,
    next: usize,
    expires: u64,
}

struct DiscoveryCursor {
    claimant: String,
    request: Value,
    context: Value,
    limits: WorkLimits,
    roots: Vec<PathBuf>,
    directories: Vec<std::fs::ReadDir>,
    seen: HashSet<SourceIdentity>,
    seen_directories: HashSet<SourceIdentity>,
    examined: usize,
    sources: usize,
    emitted: usize,
    output_bytes: usize,
    inventory: HashMap<String, Value>,
    previous: HashMap<String, Value>,
    removed: Vec<Value>,
    walking: bool,
    expires: u64,
    accounted: usize,
}

struct ResolutionCursor {
    claimant: String,
    context: Value,
    request: Value,
    ids: Vec<String>,
    paths: HashMap<String, PathBuf>,
    sessions: Vec<Value>,
    next: usize,
    pending: Option<String>,
    remaining: WorkLimits,
    complete_scan: bool,
    expires: u64,
}

struct LocatedPath {
    path: PathBuf,
    expires: u64,
}

struct LocateCursor {
    claimant: String,
    context: Value,
    limits: WorkLimits,
    ids: Vec<String>,
    wanted: HashSet<String>,
    found: HashSet<String>,
    scope: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    directories: Vec<std::fs::ReadDir>,
    seen_directories: HashSet<SourceIdentity>,
    pending: VecDeque<Value>,
    examined: usize,
    emitted: usize,
    output_bytes: usize,
    finished: bool,
    exhausted: bool,
    expires: u64,
}

impl LocateCursor {
    fn accounted_bytes(&self) -> usize {
        let context = crate::snapshot_memory::value_charge(&self.context);
        size_of::<Self>()
            + self.claimant.capacity()
            + context.owned_capacity_bytes
            + context.opaque_dom_accounted_bytes
            + self.ids.capacity() * size_of::<String>()
            + self.ids.iter().map(String::capacity).sum::<usize>()
            + self.wanted.capacity() * size_of::<String>()
            + self.wanted.iter().map(String::capacity).sum::<usize>()
            + self.found.capacity() * size_of::<String>()
            + self.found.iter().map(String::capacity).sum::<usize>()
            + self.scope.capacity() * size_of::<PathBuf>()
            + self
                .scope
                .iter()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
            + self.roots.capacity() * size_of::<PathBuf>()
            + self
                .roots
                .iter()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
            + self.directories.capacity() * size_of::<std::fs::ReadDir>()
            + self.seen_directories.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<Value>()
            + self
                .pending
                .iter()
                .map(|item| {
                    let charge = crate::snapshot_memory::value_charge(item);
                    charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                })
                .sum::<usize>()
    }
}

struct Checkpoint {
    claimant: String,
    roots: Value,
    inventory: HashMap<String, Value>,
    expires: u64,
    accounted: usize,
}

struct LabelSlot {
    preparation: crate::snapshot_labels::LabelPreparation,
    source_handle: Value,
    accounted: usize,
    expires: u64,
    admission: String,
}

struct Delivery {
    claimant: String,
    leases: Vec<String>,
    cursor: Option<String>,
    expires: u64,
}

struct GraphNode {
    path: PathBuf,
    depth: usize,
    spawned_by: Option<String>,
    snapshot: Arc<TranscriptSnapshot>,
    description: Value,
    transferred: bool,
}

enum GraphTask {
    Visit {
        path: PathBuf,
        depth: usize,
        spawned_by: Option<String>,
    },
    List {
        parent: PathBuf,
        depth: usize,
    },
}

struct GraphListing {
    entries: std::fs::ReadDir,
    children: Vec<PathBuf>,
    depth: usize,
}

struct GraphPending {
    token: String,
    path: PathBuf,
    depth: usize,
    spawned_by: Option<String>,
}

struct GraphCursor {
    claimant: String,
    context: Value,
    request: Value,
    root_handle: Value,
    remaining: WorkLimits,
    nodes: Vec<GraphNode>,
    seen: HashSet<SourceIdentity>,
    tasks: Vec<GraphTask>,
    listing: Option<GraphListing>,
    pending: Option<GraphPending>,
    prepared: bool,
    root_checked: bool,
    projection_at: usize,
    pending_record: Option<(usize, String)>,
    published_members: Vec<usize>,
    expires: u64,
    accounted: usize,
}

enum GraphYield {
    Pending(Value),
    Complete(Value),
}

struct ClassifierStage {
    activity: ActivityIndex,
    indexed: usize,
    result: Option<Arc<TranscriptSnapshot>>,
}

struct ClassifierSlot {
    work: Mutex<ClassifierStage>,
    accounted: AtomicUsize,
    deadline: u64,
    complete: AtomicBool,
}

struct ClassifierProgress {
    snapshot: Option<Arc<TranscriptSnapshot>>,
    read_bytes: usize,
    events: usize,
}

struct RegistryRecord {
    snapshot: Arc<crate::toolcall::ToolRegistrySnapshot>,
    allocations: Vec<(usize, usize)>,
}

struct GenerationRecord {
    snapshot: Weak<TranscriptSnapshot>,
    registry_generation: String,
    entries: Vec<(usize, MemoryCharge)>,
    indexes: Vec<(usize, usize)>,
}

impl GenerationRecord {
    fn new(snapshot: &Arc<TranscriptSnapshot>, registry_generation: &str) -> Self {
        Self {
            snapshot: Arc::downgrade(snapshot),
            registry_generation: registry_generation.to_owned(),
            entries: snapshot.accounted_allocations(),
            indexes: snapshot.activity.accounted_allocations(),
        }
    }
}

#[derive(Default)]
struct StoreState {
    latest: HashMap<SourceIdentity, Arc<TranscriptSnapshot>>,
    loads: HashMap<SourceIdentity, Arc<LoadSlot>>,
    leases: HashMap<String, Lease>,
    waiters: HashMap<String, Waiter>,
    projections: HashMap<String, ProjectionCursor>,
    generations: HashMap<String, GenerationRecord>,
    classifier_stages: HashMap<String, Arc<ClassifierSlot>>,
    graphs: HashMap<String, GraphCursor>,
    deliveries: HashMap<String, Delivery>,
    labels: HashMap<String, LabelSlot>,
    registries: HashMap<String, RegistryRecord>,
    discoveries: HashMap<String, DiscoveryCursor>,
    checkpoints: HashMap<String, Checkpoint>,
    resolutions: HashMap<String, ResolutionCursor>,
    locations: HashMap<String, LocatedPath>,
    locates: HashMap<String, LocateCursor>,
    escaped_chunks: HashMap<usize, (Weak<Vec<Entry>>, MemoryCharge)>,
    counters: [u64; 18],
    transient_bytes: usize,
    owned: crate::snapshot_owned::OwnedProjections,
}

pub type ClassifierCallback =
    dyn Fn(&[Arc<EntryChunk>], Range<usize>) -> Result<Vec<bool>, SnapshotError> + Send + Sync;

pub type PolicyCallback = dyn Fn(
        Arc<TranscriptSnapshot>,
        &Value,
        &WorkLimits,
        &Cancellation,
        usize,
    ) -> Result<Projection, SnapshotError>
    + Send
    + Sync;

pub struct NativeStore {
    config: Config,
    pub owner_epoch: String,
    default_registry: String,
    seed: [u8; 32],
    sequence: AtomicUsize,
    state: Mutex<StoreState>,
    classifiers: Mutex<HashMap<String, Arc<ClassifierCallback>>>,
    policies: Mutex<HashMap<String, Arc<PolicyCallback>>>,
    classified: Mutex<HashMap<String, Arc<TranscriptSnapshot>>>,
    #[cfg(test)]
    read_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

pub(crate) struct ProjectionReservation<'a> {
    store: &'a NativeStore,
    bytes: usize,
}

impl Drop for ProjectionReservation<'_> {
    fn drop(&mut self) {
        self.store
            .state
            .lock()
            .expect("snapshot state")
            .transient_bytes -= self.bytes;
    }
}

pub(crate) struct ProjectionArena<'a> {
    state: Arc<Mutex<ProjectionArenaState<'a>>>,
}

struct ProjectionArenaState<'a> {
    reservation: ProjectionReservation<'a>,
    context: Value,
    live: usize,
    peak: usize,
    admissions: usize,
}

pub struct ProjectionAllocation<'a> {
    arena: Arc<Mutex<ProjectionArenaState<'a>>>,
    bytes: usize,
}

impl Drop for ProjectionAllocation<'_> {
    fn drop(&mut self) {
        let mut state = self.arena.lock().expect("projection arena");
        state.live = state
            .live
            .checked_sub(self.bytes)
            .expect("balanced projection allocations");
    }
}

impl<'a> ProjectionArena<'a> {
    pub(crate) fn new(store: &'a NativeStore, context: Value) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProjectionArenaState {
                reservation: ProjectionReservation { store, bytes: 0 },
                context,
                live: 0,
                peak: 0,
                admissions: 0,
            })),
        }
    }

    pub(crate) fn allocate(&self, bytes: usize) -> Result<ProjectionAllocation<'a>, SnapshotError> {
        self.grow(bytes)?;
        Ok(ProjectionAllocation {
            arena: self.state.clone(),
            bytes,
        })
    }

    pub(crate) fn extend(
        &self,
        allocation: &mut ProjectionAllocation<'a>,
        bytes: usize,
    ) -> Result<(), SnapshotError> {
        if !Arc::ptr_eq(&self.state, &allocation.arena) {
            return Err(invalid("projection allocation belongs to another arena"));
        }
        let total = allocation
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("projection allocation overflow"))?;
        self.grow(bytes)?;
        allocation.bytes = total;
        Ok(())
    }

    fn grow(&self, bytes: usize) -> Result<(), SnapshotError> {
        let mut state = self.state.lock().expect("projection arena");
        let required = state
            .live
            .checked_add(bytes)
            .ok_or_else(|| invalid("projection arena overflow"))?;
        if required > state.reservation.bytes {
            let preferred = required
                .checked_next_power_of_two()
                .unwrap_or(required)
                .max(4096);
            let ProjectionArenaState {
                reservation,
                context,
                ..
            } = &mut *state;
            reservation.store.grow_projection_capacity(
                reservation,
                context,
                required,
                preferred,
            )?;
            state.admissions += 1;
        }
        state.live = required;
        state.peak = state.peak.max(required);
        Ok(())
    }

    pub(crate) fn counters(&self) -> (usize, usize, usize) {
        let state = self.state.lock().expect("projection arena");
        (state.reservation.bytes, state.peak, state.admissions)
    }
}

struct WaiterClaim<'a> {
    store: &'a NativeStore,
    token: &'a str,
    keep: bool,
}

impl Drop for WaiterClaim<'_> {
    fn drop(&mut self) {
        let mut state = self.store.state.lock().expect("snapshot state");
        if self.keep {
            if let Some(waiter) = state.waiters.get_mut(self.token) {
                waiter.busy = false;
            }
        } else {
            state.waiters.remove(self.token);
        }
        NativeStore::prune(&mut state);
    }
}

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing string {key}")))
}

fn classifier_eq(left: &Value, right: &Value) -> Result<bool, SnapshotError> {
    Ok(str_field(left, "id")? == str_field(right, "id")?
        && str_field(left, "version")? == str_field(right, "version")?)
}

fn number(value: &Value, key: &str) -> Result<usize, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| invalid(format!("missing count {key}")))
}

fn io_error(error: std::io::Error) -> SnapshotError {
    let status = match error.kind() {
        std::io::ErrorKind::NotFound => Status::Missing,
        std::io::ErrorKind::PermissionDenied => Status::PermissionDenied,
        _ => Status::Changed,
    };
    SnapshotError::new(status, error.to_string())
}

fn encoded_size(value: &Value, limit: usize) -> Result<usize, SnapshotError> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("reply output limit"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    crate::snapshot_codec::write_json(&mut counter, value, limit)
        .map_err(|_| SnapshotError::new(Status::OutputLimit, "encoded reply exceeds page bound"))?;
    Ok(counter.bytes)
}

fn usage_value(usage: &[u64; 18]) -> Value {
    let mut result = json!({});
    for (key, count) in COUNTERS.iter().zip(usage.iter()).take(18) {
        result.insert(*key, json!(*count));
    }
    result
}

fn limits(request: &Value) -> Result<WorkLimits, SnapshotError> {
    let value = request
        .get("limits")
        .ok_or_else(|| invalid("missing limits"))?;
    Ok(WorkLimits {
        max_read_bytes: number(value, "max_read_bytes")?,
        max_events: number(value, "max_events")?,
        max_items: number(value, "max_items")?,
        max_output_bytes: number(value, "max_output_bytes")?,
        max_discovery_entries: number(value, "max_discovery_entries")?,
        max_sources: number(value, "max_sources")?,
        deadline_unix_ms: number(request, "deadline_unix_ms")? as u64,
    })
}

impl NativeStore {
    pub fn new(value: &Value) -> Result<Self, SnapshotError> {
        let get = |key, default| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .map_or(default, |n| n as usize)
        };
        let config = Config {
            retained: get("max_retained_bytes", 1024 * 1024 * 1024),
            source: get("max_source_bytes", 512 * 1024 * 1024),
            entry: get("max_entry_bytes", 64 * 1024 * 1024),
            output: get("max_projection_bytes", 16 * 1024 * 1024),
            leases: get("max_leases", 256),
            ttl: get("max_lease_ms", 30_000) as u64,
            preparation: get("max_preparation_ms", 120_000) as u64,
            loads: get("max_pending_loads", 4),
            read_step: get("max_read_bytes_per_step", 8 * 1024 * 1024),
            event_step: get("max_events_per_step", 4096),
            page_items: get("max_items_per_page", 256).min(256),
            hook_loads: get("reserved_hook_loads", 1),
            hook_leases: get("reserved_hook_leases", 32),
            hook_bytes: get("reserved_hook_accounted_bytes", 512 * 1024 * 1024),
        };
        if config.retained == 0
            || config.entry == 0
            || config.read_step == 0
            || config.event_step == 0
            || config.page_items == 0
            || config.leases == 0
            || config.loads == 0
            || config.ttl == 0
            || config.preparation == 0
            || config.source == 0
            || config.output == 0
        {
            return Err(invalid("zero store bound"));
        }
        let mut seed = [0u8; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut seed))
            .map_err(io_error)?;
        let owner_epoch = format!("{:x}", Sha256::digest(seed));
        let captured = crate::toolcall::ToolRegistrySnapshot::capture_current();
        let default_registry = captured.fingerprint().to_owned();
        let builtin = crate::toolcall::ToolRegistrySnapshot::from_specs(HashMap::new());
        let mut state = StoreState::default();
        for registry in [captured, builtin] {
            state
                .registries
                .entry(registry.fingerprint().to_owned())
                .or_insert_with(|| RegistryRecord {
                    allocations: registry.accounted_allocations(),
                    snapshot: registry,
                });
        }
        Ok(Self {
            config,
            owner_epoch,
            default_registry,
            seed,
            sequence: AtomicUsize::new(1),
            state: Mutex::new(state),
            classifiers: Mutex::new(HashMap::new()),
            policies: Mutex::new(HashMap::new()),
            classified: Mutex::new(HashMap::new()),
            #[cfg(test)]
            read_hook: Mutex::new(None),
        })
    }

    pub fn scan_limits(&self) -> WorkLimits {
        WorkLimits {
            max_read_bytes: self.config.read_step,
            max_events: self.config.event_step,
            max_items: self.config.event_step,
            max_output_bytes: self.config.output,
            max_discovery_entries: self.config.event_step,
            max_sources: self.config.page_items,
            deadline_unix_ms: now_ms().saturating_add(self.config.preparation),
        }
    }

    pub fn default_registry_generation(&self) -> String {
        self.default_registry.clone()
    }

    pub fn register_tool_registry(
        &self,
        specs: &Value,
        context: &Value,
    ) -> Result<String, SnapshotError> {
        self.authority(context, None)?;
        let charge = crate::snapshot_memory::value_charge(specs);
        let reserve = (charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
            .saturating_mul(4)
            .saturating_add(64 * 1024);
        let mut reservation = self.reserve_projection(context, reserve)?;
        let registry =
            crate::toolcall::ToolRegistrySnapshot::from_specs_json(specs).map_err(invalid)?;
        let fingerprint = registry.fingerprint().to_owned();
        let allocations = registry.accounted_allocations();
        let bytes = allocations.iter().map(|(_, bytes)| *bytes).sum::<usize>()
            + allocations.capacity() * size_of::<(usize, usize)>()
            + size_of::<RegistryRecord>()
            + fingerprint.capacity();
        let mut state = self.state.lock().expect("snapshot state");
        if !state.registries.contains_key(&fingerprint) {
            if bytes > reservation.bytes {
                return Err(SnapshotError::new(
                    Status::RetainedLimit,
                    "tool registry allocation exceeds reservation",
                ));
            }
            state.registries.insert(
                fingerprint.clone(),
                RegistryRecord {
                    snapshot: registry,
                    allocations,
                },
            );
            state.transient_bytes -= bytes;
            reservation.bytes -= bytes;
        }
        Ok(fingerprint)
    }

    pub(crate) fn registry(
        &self,
        context: &Value,
    ) -> Result<Arc<crate::toolcall::ToolRegistrySnapshot>, SnapshotError> {
        self.state
            .lock()
            .expect("snapshot state")
            .registries
            .get(str_field(context, "registry_generation")?)
            .map(|record| Arc::clone(&record.snapshot))
            .ok_or_else(|| invalid("tool registry generation is not registered with this owner"))
    }

    pub fn registry_for_scope(
        &self,
        handle: &Value,
        context: &Value,
    ) -> Result<Arc<crate::toolcall::ToolRegistrySnapshot>, SnapshotError> {
        let state = self.state.lock().expect("snapshot state");
        Ok(Arc::clone(&self.lease(&state, handle, context)?.registry))
    }

    pub(crate) fn owned_token(&self) -> String {
        format!("domain-projection:{}", self.token("owned-domain"))
    }

    pub(crate) fn owned_handle_expiry(
        &self,
        handles: &[Value],
        context: &Value,
        deadline: u64,
    ) -> Result<u64, SnapshotError> {
        for handle in handles {
            self.pin_scope(handle, context)?;
        }
        let state = self.state.lock().expect("snapshot state");
        let mut expires = (now_ms() + self.config.ttl).min(deadline);
        for handle in handles {
            expires = expires.min(self.lease(&state, handle, context)?.expires);
        }
        Ok(expires)
    }

    pub(crate) fn reserve_projection(
        &self,
        context: &Value,
        bytes: usize,
    ) -> Result<ProjectionReservation<'_>, SnapshotError> {
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        self.admit_memory(&mut state, context, bytes)?;
        state.transient_bytes += bytes;
        Ok(ProjectionReservation { store: self, bytes })
    }

    pub(crate) fn extend_projection_reservation(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        bytes: usize,
    ) -> Result<(), SnapshotError> {
        if !std::ptr::eq(self, reservation.store) {
            return Err(invalid("projection reservation belongs to another owner"));
        }
        let mut state = self.state.lock().expect("snapshot state");
        self.admit_memory(&mut state, context, bytes)?;
        state.transient_bytes += bytes;
        reservation.bytes += bytes;
        Ok(())
    }

    fn grow_projection_capacity(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        required: usize,
        preferred: usize,
    ) -> Result<(), SnapshotError> {
        if !std::ptr::eq(self, reservation.store) {
            return Err(invalid("projection reservation belongs to another owner"));
        }
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        let additional = required - reservation.bytes;
        self.admit_memory(&mut state, context, additional)?;
        let cap = self.memory_cap(context)?;
        let used = number(&Self::gauges(&state), "retained_total_accounted_bytes")?;
        let available = cap.saturating_sub(used);
        let preferred_growth = preferred.saturating_sub(reservation.bytes);
        let growth = preferred_growth.min(available);
        state.transient_bytes += growth;
        reservation.bytes += growth;
        Ok(())
    }

    pub(crate) fn with_owned<T>(
        &self,
        operation: impl FnOnce(&mut crate::snapshot_owned::OwnedProjections) -> Result<T, SnapshotError>,
    ) -> Result<T, SnapshotError> {
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        operation(&mut state.owned)
    }

    pub(crate) fn take_owned<T>(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        retained_bytes: usize,
        operation: impl FnOnce(&mut crate::snapshot_owned::OwnedProjections) -> Result<T, SnapshotError>,
    ) -> Result<T, SnapshotError> {
        if !std::ptr::eq(self, reservation.store) {
            return Err(invalid("projection reservation belongs to another owner"));
        }
        let mut state = self.state.lock().expect("snapshot state");
        let result = operation(&mut state.owned)?;
        state.transient_bytes += retained_bytes;
        reservation.bytes += retained_bytes;
        Ok(result)
    }

    pub(crate) fn publish_owned_bound<T>(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        retained_bytes: usize,
        handles: &[Value],
        context: &Value,
        deadline: u64,
        operation: impl FnOnce(&mut crate::snapshot_owned::OwnedProjections) -> Result<T, SnapshotError>,
    ) -> Result<T, SnapshotError> {
        if !std::ptr::eq(self, reservation.store) || retained_bytes > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "owned publication bytes were not reserved",
            ));
        }
        let mut state = self.state.lock().expect("snapshot state");
        if now_ms() >= deadline {
            return Err(SnapshotError::new(
                Status::Deadline,
                "owned projection deadline expired",
            ));
        }
        for handle in handles {
            self.lease(&state, handle, context)?;
        }
        let result = operation(&mut state.owned)?;
        state.transient_bytes -= retained_bytes;
        reservation.bytes -= retained_bytes;
        Ok(result)
    }

    pub(crate) fn publish_owned<T>(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        retained_bytes: usize,
        operation: impl FnOnce(&mut crate::snapshot_owned::OwnedProjections) -> Result<T, SnapshotError>,
    ) -> Result<T, SnapshotError> {
        if !std::ptr::eq(self, reservation.store) || retained_bytes > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "owned publication bytes were not reserved",
            ));
        }
        let mut state = self.state.lock().expect("snapshot state");
        let result = operation(&mut state.owned)?;
        state.transient_bytes -= retained_bytes;
        reservation.bytes -= retained_bytes;
        Ok(result)
    }

    fn label_reply(
        &self,
        mut reply: Value,
        context: &Value,
        work: crate::snapshot_labels::LabelUsage,
        activity_lifts: usize,
    ) -> Result<Value, SnapshotError> {
        let mut usage = [0u64; 18];
        usage[6] = activity_lifts as u64;
        reply.insert("work",json!({"events":work.events,"input_bytes":work.input_bytes,"items":work.items,"output_bytes":work.output_bytes}));
        reply.insert("usage", usage_value(&usage));
        self.track_delivery(&reply, context, true);
        for _ in 0..3 {
            match encoded_size(&reply, MAX_REPLY_BYTES) {
                Ok(bytes) => usage[13] = bytes as u64,
                Err(error) => {
                    self.discard_response(&reply, context)?;
                    return Err(error);
                }
            }
            reply.insert("usage", usage_value(&usage));
        }
        let mut state = self.state.lock().expect("snapshot state");
        for (total, own) in state.counters.iter_mut().zip(usage.iter()) {
            *total += own;
        }
        Ok(reply)
    }

    fn publish_label(
        &self,
        token: String,
        mut slot: LabelSlot,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(), SnapshotError> {
        let handle_charge = crate::snapshot_memory::value_charge(&slot.source_handle);
        slot.accounted = slot.preparation.accounted_bytes()
            + size_of::<LabelSlot>()
            + token.capacity()
            + handle_charge.owned_capacity_bytes
            + handle_charge.opaque_dom_accounted_bytes;
        if slot.accounted > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "classifier retained bytes exceed reservation",
            ));
        }
        let mut state = self.state.lock().expect("snapshot state");
        let lease = self.lease(&state, &slot.source_handle, context)?;
        slot.expires = slot.expires.min(lease.expires);
        if slot.expires <= now_ms() {
            return Err(SnapshotError::new(
                Status::Deadline,
                "classifier preparation expired",
            ));
        }
        if state.labels.len() >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "classifier cursor admission exhausted",
            ));
        }
        state.transient_bytes -= slot.accounted;
        reservation.bytes -= slot.accounted;
        state.labels.insert(token, slot);
        Ok(())
    }

    fn publish_label_generation(
        &self,
        snapshot: Arc<TranscriptSnapshot>,
        generation: GenerationRecord,
        classifier: Value,
        source_handle: &Value,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Value, SnapshotError> {
        let shared: HashSet<_> = snapshot
            .chunks
            .iter()
            .map(|chunk| Arc::as_ptr(&chunk.entries) as usize)
            .collect();
        let retained = generation
            .entries
            .iter()
            .filter(|(id, _)| !shared.contains(id))
            .map(|(_, charge)| charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
            .sum::<usize>()
            + generation
                .indexes
                .iter()
                .map(|(_, bytes)| *bytes)
                .sum::<usize>()
            + generation.entries.capacity() * size_of::<(usize, MemoryCharge)>()
            + generation.indexes.capacity() * size_of::<(usize, usize)>()
            + size_of::<GenerationRecord>()
            + snapshot.id.capacity();
        if retained > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "derived classifier publication exceeds reservation",
            ));
        }
        let mut state = self.state.lock().expect("snapshot state");
        let absolute_deadline = self
            .lease(&state, source_handle, context)?
            .absolute_deadline;
        let id = snapshot.id.clone();
        state.generations.insert(id.clone(), generation);
        let data = match self.issue(&mut state, snapshot, classifier, context, absolute_deadline) {
            Ok(data) => data,
            Err(error) => {
                state.generations.remove(&id);
                return Err(error);
            }
        };
        state.transient_bytes -= retained;
        reservation.bytes -= retained;
        Ok(data)
    }

    pub fn prepare_classifier(
        &self,
        handle: &Value,
        classifier: &Value,
        context: &Value,
        cancel: &Cancellation,
        bounds: WorkLimits,
    ) -> Result<Value, SnapshotError> {
        let result = self.prepare_classifier_inner(handle, classifier, context, cancel, bounds);
        if let Err(error) = &result {
            self.state.lock().expect("snapshot state").counters[if error.status == Status::Cancelled
            {
                11
            } else {
                12
            }] += 1;
        }
        result
    }

    fn prepare_classifier_inner(
        &self,
        handle: &Value,
        classifier: &Value,
        context: &Value,
        cancel: &Cancellation,
        mut bounds: WorkLimits,
    ) -> Result<Value, SnapshotError> {
        cancel.check(bounds.deadline_unix_ms)?;
        let (source, description) =
            self.pin_scope_for_work(handle, context, bounds.deadline_unix_ms)?;
        bounds.deadline_unix_ms = bounds
            .deadline_unix_ms
            .min(number(&description, "lease_expires_unix_ms")? as u64);
        bounds.deadline_unix_ms = bounds
            .deadline_unix_ms
            .min(now_ms() + self.config.preparation);
        bounds.max_output_bytes = bounds.max_output_bytes.min(self.config.output);
        let binding = crate::snapshot_labels::LabelBinding {
            owner_epoch: self.owner_epoch.clone(),
            claimant: str_field(context, "claimant")?.to_owned(),
            physical_generation: source.id.clone(),
            classifier_id: str_field(classifier, "id")?.to_owned(),
            classifier_version: str_field(classifier, "version")?.to_owned(),
            registry_generation: str_field(context, "registry_generation")?.to_owned(),
            execution_id: self.token("label-execution"),
        };
        let entry_bytes: usize = source
            .chunks
            .iter()
            .map(|chunk| {
                chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes
            })
            .sum();
        let max_stage_bytes = entry_bytes
            .saturating_mul(4)
            .saturating_add(source.event_count.saturating_mul(512))
            .saturating_add(16 * 1024)
            .min(self.config.retained);
        let mut reservation = self.reserve_projection(
            context,
            crate::snapshot_labels::LabelPreparation::initial_reservation_bytes(&binding),
        )?;
        let expires = self.owned_handle_expiry(
            std::slice::from_ref(handle),
            context,
            bounds.deadline_unix_ms,
        )?;
        let mut preparation = crate::snapshot_labels::LabelPreparation::new(
            source,
            binding.clone(),
            bounds,
            max_stage_bytes,
        )?;
        if preparation.source().event_count == 0 {
            let work = preparation.usage();
            let classifier = preparation.derived_classifier();
            let snapshot = preparation.finish(&binding, cancel)?;
            let generation = GenerationRecord::new(&snapshot, &binding.registry_generation);
            let data = self.publish_label_generation(
                snapshot,
                generation,
                classifier,
                handle,
                context,
                &mut reservation,
            )?;
            return self.label_reply(
                json!({"complete":true,"cursor":null,"description":data["description"]}),
                context,
                work,
                0,
            );
        }
        let token = format!("classifier:{}", self.token("label-page"));
        let page = preparation.next_page(token.clone(), &binding, cancel)?;
        let work = preparation.usage();
        let reply = json!({"complete":false,"cursor":page.cursor,"record_schema":page.record_schema,"records_json":page.records_json,"event_start":page.event_start});
        self.publish_label(
            token,
            LabelSlot {
                preparation,
                source_handle: handle.clone(),
                accounted: 0,
                expires,
                admission: str_field(context, "admission")?.to_owned(),
            },
            context,
            &mut reservation,
        )?;
        self.label_reply(reply, context, work, 0)
    }

    pub fn submit_classifier(
        &self,
        cursor: &str,
        labels: &[bool],
        context: &Value,
        cancel: &Cancellation,
    ) -> Result<Value, SnapshotError> {
        let result = self.submit_classifier_inner(cursor, labels, context, cancel);
        if let Err(error) = &result {
            self.state.lock().expect("snapshot state").counters[if error.status == Status::Cancelled
            {
                11
            } else {
                12
            }] += 1;
        }
        result
    }

    fn submit_classifier_inner(
        &self,
        cursor: &str,
        labels: &[bool],
        context: &Value,
        cancel: &Cancellation,
    ) -> Result<Value, SnapshotError> {
        self.authority(context, None)?;
        let (source_handle, deadline) = {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            let slot = state.labels.get(cursor).ok_or_else(|| {
                SnapshotError::new(Status::StaleCursor, "classifier cursor expired or consumed")
            })?;
            if slot.preparation.binding().claimant != str_field(context, "claimant")? {
                return Err(SnapshotError::new(
                    Status::PermissionDenied,
                    "classifier cursor claimant differs",
                ));
            }
            if slot.admission != str_field(context, "admission")? {
                return Err(SnapshotError::new(
                    Status::StaleCursor,
                    "classifier admission differs",
                ));
            }
            if slot.preparation.binding().registry_generation
                != str_field(context, "registry_generation")?
            {
                return Err(SnapshotError::new(
                    Status::StaleCursor,
                    "classifier registry generation differs",
                ));
            }
            (
                slot.source_handle.clone(),
                slot.preparation.remaining_limits().deadline_unix_ms,
            )
        };
        if let Err(error) = cancel.check(deadline) {
            self.release_cursor(cursor, context)?;
            return Err(error);
        }
        self.pin(&source_handle, context)?;
        let mut reservation = self.reserve_projection(context, 0)?;
        let mut slot = {
            let mut state = self.state.lock().expect("snapshot state");
            self.lease(&state, &source_handle, context)?;
            let slot = state.labels.remove(cursor).ok_or_else(|| {
                SnapshotError::new(Status::StaleCursor, "classifier cursor already consumed")
            })?;
            state.transient_bytes += slot.accounted;
            reservation.bytes += slot.accounted;
            slot
        };
        let working = slot.preparation.next_operation_reservation_bytes();
        self.extend_projection_reservation(&mut reservation, context, working)?;
        let binding = slot.preparation.binding().clone();
        let before = slot.preparation.usage();
        let registry = self.registry_for_scope(&source_handle, context)?;
        let complete = crate::toolcall::with_registry(registry, || {
            slot.preparation.submit(cursor, labels, &binding, cancel)
        })?;
        if complete {
            let work = slot.preparation.usage();
            let classifier = slot.preparation.derived_classifier();
            let snapshot = slot.preparation.finish(&binding, cancel)?;
            let generation = GenerationRecord::new(&snapshot, &binding.registry_generation);
            let data = self.publish_label_generation(
                snapshot,
                generation,
                classifier,
                &source_handle,
                context,
                &mut reservation,
            )?;
            self.label_reply(
                json!({"complete":true,"cursor":null,"description":data["description"]}),
                context,
                work,
                work.activity_lifts - before.activity_lifts,
            )
        } else {
            let token = format!("classifier:{}", self.token("label-page"));
            let page = slot
                .preparation
                .next_page(token.clone(), &binding, cancel)?;
            let work = slot.preparation.usage();
            let reply = json!({"complete":false,"cursor":page.cursor,"record_schema":page.record_schema,"records_json":page.records_json,"event_start":page.event_start});
            slot.expires =
                self.owned_handle_expiry(std::slice::from_ref(&source_handle), context, deadline)?;
            self.publish_label(token, slot, context, &mut reservation)?;
            self.label_reply(
                reply,
                context,
                work,
                work.activity_lifts - before.activity_lifts,
            )
        }
    }

    pub fn record_transport(&self, bytes: usize) {
        self.state.lock().expect("snapshot state").counters[14] += bytes as u64;
    }

    pub fn register_policy(
        &self,
        id: &str,
        version: &str,
        callback: Arc<PolicyCallback>,
    ) -> Result<(), SnapshotError> {
        let key = sonic_rs::to_string(&json!([id, version])).expect("policy key");
        let mut policies = self.policies.lock().expect("policies");
        if policies.contains_key(&key) {
            return Err(invalid("policy version is already registered"));
        }
        policies.insert(key, callback);
        Ok(())
    }

    pub fn register_classifier(
        &self,
        id: &str,
        version: &str,
        callback: Arc<ClassifierCallback>,
    ) -> Result<(), SnapshotError> {
        if id == "native" {
            return Err(invalid("native classifier is reserved"));
        }
        let key = sonic_rs::to_string(&json!([id, version])).expect("classifier key");
        let mut classifiers = self.classifiers.lock().expect("classifiers");
        if classifiers.contains_key(&key) {
            return Err(invalid("classifier version is already registered"));
        }
        classifiers.insert(key, callback);
        Ok(())
    }

    fn classify(
        &self,
        snapshot: Arc<TranscriptSnapshot>,
        classifier: &Value,
        context: &Value,
        cancel: &Cancellation,
        bounds: &WorkLimits,
        usage: &mut [u64; 18],
    ) -> Result<ClassifierProgress, SnapshotError> {
        let id = str_field(classifier, "id")?;
        let version = str_field(classifier, "version")?;
        let native = id == "native" && version == "1";
        let registry = str_field(context, "registry_generation")?;
        if native
            && self
                .state
                .lock()
                .expect("snapshot state")
                .generations
                .get(&snapshot.id)
                .is_some_and(|generation| generation.registry_generation == registry)
        {
            return Ok(ClassifierProgress {
                snapshot: Some(snapshot),
                read_bytes: 0,
                events: 0,
            });
        }
        let classifier_key = sonic_rs::to_string(&json!([id, version])).expect("classifier key");
        let callback = if native {
            None
        } else {
            Some(
                self.classifiers
                    .lock()
                    .expect("classifiers")
                    .get(&classifier_key)
                    .cloned()
                    .ok_or_else(|| {
                        invalid("classifier version is not registered with this owner")
                    })?,
            )
        };
        let key = sonic_rs::to_string(&json!([
            snapshot.id,
            classifier_key,
            str_field(context, "registry_generation")?
        ]))
        .expect("classified key");
        if let Some(derived) = self
            .classified
            .lock()
            .expect("classified snapshots")
            .get(&key)
            .cloned()
        {
            return Ok(ClassifierProgress {
                snapshot: Some(derived),
                read_bytes: 0,
                events: 0,
            });
        }
        let slot = {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            if let Some(slot) = state.classifier_stages.get(&key) {
                Arc::clone(slot)
            } else {
                let cap = if str_field(context, "admission")? == "hook" {
                    self.config.loads
                } else {
                    self.config.loads.saturating_sub(self.config.hook_loads)
                };
                if state.classifier_stages.len() >= cap {
                    return Err(SnapshotError::new(
                        Status::RetainedLimit,
                        "classifier preparation admission exhausted",
                    ));
                }
                self.admit_memory(
                    &mut state,
                    context,
                    size_of::<ClassifierSlot>() + size_of::<ClassifierStage>(),
                )?;
                let slot = Arc::new(ClassifierSlot {
                    work: Mutex::new(ClassifierStage {
                        activity: ActivityIndex::default(),
                        indexed: 0,
                        result: None,
                    }),
                    accounted: AtomicUsize::new(0),
                    deadline: now_ms() + self.config.preparation,
                    complete: AtomicBool::new(false),
                });
                state
                    .classifier_stages
                    .insert(key.clone(), Arc::clone(&slot));
                slot
            }
        };
        cancel.check(slot.deadline.min(bounds.deadline_unix_ms))?;
        let Ok(mut stage) = slot.work.try_lock() else {
            return Ok(ClassifierProgress {
                snapshot: None,
                read_bytes: 0,
                events: 0,
            });
        };
        if let Some(result) = &stage.result {
            return Ok(ClassifierProgress {
                snapshot: Some(Arc::clone(result)),
                read_bytes: 0,
                events: 0,
            });
        }
        let start = stage.indexed;
        let stop = snapshot
            .event_count
            .min(start + self.config.event_step.min(bounds.max_events));
        if start < snapshot.event_count && stop == start {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "classifier event budget exhausted",
            ));
        }
        let mut bytes = 0usize;
        for position in start..stop {
            let at = snapshot
                .chunks
                .partition_point(|chunk| chunk.start <= position)
                - 1;
            let chunk = &snapshot.chunks[at];
            let charge = chunk.entry_charges[position - chunk.start];
            bytes = bytes
                .saturating_add(charge.owned_capacity_bytes)
                .saturating_add(charge.opaque_dom_accounted_bytes);
            if bytes > bounds.max_read_bytes {
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "classifier read budget exhausted before callback",
                ));
            }
        }
        let reserve = bytes.saturating_mul(2).saturating_add(stop - start);
        {
            let mut state = self.state.lock().expect("snapshot state");
            self.admit_memory(&mut state, context, reserve)?;
            slot.accounted.fetch_add(reserve, Ordering::AcqRel);
        }
        if stop > start {
            let flags = if let Some(callback) = &callback {
                let flags = callback(&snapshot.chunks, start..stop)?;
                if flags.len() != stop - start {
                    return Err(invalid("classifier returned a mismatched flag count"));
                }
                Some(flags)
            } else {
                None
            };
            stage.activity = std::mem::take(&mut stage.activity)
                .append_tail(&snapshot.range(start..stop), flags.as_deref());
            stage.indexed = stop;
            usage[6] += 1;
        }
        slot.accounted.store(
            stage.activity.accounted_bytes() + size_of::<ClassifierStage>(),
            Ordering::Release,
        );
        cancel.check(slot.deadline.min(bounds.deadline_unix_ms))?;
        if stop < snapshot.event_count {
            return Ok(ClassifierProgress {
                snapshot: None,
                read_bytes: bytes,
                events: stop - start,
            });
        }
        let derived = Arc::new(TranscriptSnapshot {
            id: self.token("classified"),
            canonical_path: snapshot.canonical_path.clone(),
            stamp: snapshot.stamp,
            provider: snapshot.provider,
            session_id: snapshot.session_id.clone(),
            chunks: snapshot.chunks.clone(),
            activity: Arc::new(std::mem::take(&mut stage.activity)),
            committed_bytes: snapshot.committed_bytes,
            provisional_tail: snapshot.provisional_tail,
            fence: snapshot.fence.clone(),
            event_count: snapshot.event_count,
        });
        let generation = GenerationRecord::new(&derived, registry);
        {
            let mut state = self.state.lock().expect("snapshot state");
            state.generations.insert(derived.id.clone(), generation);
            slot.accounted.store(0, Ordering::Release);
            self.admit_memory(&mut state, context, 0)?;
        }
        stage.result = Some(Arc::clone(&derived));
        slot.complete.store(true, Ordering::Release);
        self.classified
            .lock()
            .expect("classified snapshots")
            .insert(key, Arc::clone(&derived));
        Ok(ClassifierProgress {
            snapshot: Some(derived),
            read_bytes: bytes,
            events: stop - start,
        })
    }

    fn token(&self, kind: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(self.seed);
        digest.update(kind.as_bytes());
        digest.update(self.sequence.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        format!("{:x}", digest.finalize())
    }

    fn authority(&self, context: &Value, path: Option<&Path>) -> Result<(), SnapshotError> {
        let authority = context
            .get("authority")
            .ok_or_else(|| invalid("missing authority"))?;
        let uid = str_field(authority, "effective_uid")?
            .parse::<u32>()
            .map_err(|_| invalid("invalid effective uid"))?;
        if uid != unsafe { libc::geteuid() } {
            return Err(SnapshotError::new(
                Status::PermissionDenied,
                "effective uid differs from owner",
            ));
        }
        match str_field(authority, "kind")? {
            "user" => Ok(()),
            "restricted_roots" => {
                let Some(path) = path else {
                    return Ok(());
                };
                let roots = authority
                    .get("roots")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("missing roots"))?;
                for root in roots.iter() {
                    let root = std::fs::canonicalize(
                        root.as_str().ok_or_else(|| invalid("invalid root"))?,
                    )
                    .map_err(io_error)?;
                    if path.starts_with(root) {
                        return Ok(());
                    }
                }
                Err(SnapshotError::new(
                    Status::PermissionDenied,
                    "source is outside authorized roots",
                ))
            }
            _ => Err(invalid("unknown authority kind")),
        }
    }

    fn prune(state: &mut StoreState) {
        let now = now_ms();
        state
            .deliveries
            .retain(|_, delivery| delivery.expires > now);
        state.owned.prune(now);
        let expired_graphs: Vec<_> = state
            .graphs
            .iter()
            .filter(|(_, graph)| graph.expires <= now || graph.remaining.deadline_unix_ms <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired_graphs {
            if let Some(graph) = state.graphs.remove(&token) {
                Self::release_graph_state(state, &graph);
            }
        }
        state.classifier_stages.retain(|_, slot| {
            Arc::strong_count(slot) > 1
                || slot.deadline > now && !slot.complete.load(Ordering::Acquire)
        });
        state
            .discoveries
            .retain(|_, cursor| cursor.expires > now && cursor.limits.deadline_unix_ms > now);
        state
            .checkpoints
            .retain(|_, checkpoint| checkpoint.expires > now);
        state
            .resolutions
            .retain(|_, cursor| cursor.expires > now && cursor.remaining.deadline_unix_ms > now);
        state.locations.retain(|_, location| location.expires > now);
        state
            .locates
            .retain(|_, cursor| cursor.expires > now && cursor.limits.deadline_unix_ms > now);
        state.leases.retain(|_, lease| lease.expires > now);
        state
            .waiters
            .retain(|_, waiter| waiter.expires > now && waiter.deadline > now);
        state
            .generations
            .retain(|_, record| record.snapshot.strong_count() > 0);
        state
            .escaped_chunks
            .retain(|_, (chunk, _)| chunk.strong_count() > 0);
        state
            .projections
            .retain(|_, cursor| cursor.expires > now && cursor.limits.deadline_unix_ms > now);
        let active: HashSet<_> = state
            .waiters
            .values()
            .map(|w| w.load.stamp.identity)
            .collect();
        state.loads.retain(|id, slot| {
            (active.contains(id) && slot.deadline > now) || Arc::strong_count(slot) > 1
        });
    }

    fn gauges(state: &StoreState) -> Value {
        let mut allocations = HashSet::new();
        let mut snapshots = HashSet::new();
        let mut entries = 0usize;
        let mut indexes = 0usize;
        for record in state.generations.values() {
            let Some(snapshot) = record.snapshot.upgrade() else {
                continue;
            };
            snapshots.insert(Arc::as_ptr(&snapshot) as usize);
            for (id, charge) in &record.entries {
                if allocations.insert(*id) {
                    entries += charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                }
            }
            for (id, charge) in &record.indexes {
                if allocations.insert(*id) {
                    indexes += *charge;
                }
            }
        }
        for registry in state.registries.values() {
            for (id, bytes) in &registry.allocations {
                if allocations.insert(*id) {
                    indexes += bytes;
                }
            }
        }
        for (chunk, charge) in state.escaped_chunks.values() {
            if let Some(chunk) = chunk.upgrade() {
                if allocations.insert(Arc::as_ptr(&chunk) as usize) {
                    entries += charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                }
            }
        }
        let label_bytes: usize = state.labels.values().map(|slot| slot.accounted).sum();
        let classifier_bytes: usize = state
            .classifier_stages
            .values()
            .map(|slot| slot.accounted.load(Ordering::Acquire))
            .sum();
        let pending: usize = state
            .loads
            .values()
            .map(|slot| slot.accounted.load(Ordering::Acquire))
            .sum();
        let discovery_bytes: usize = state
            .discoveries
            .values()
            .map(|cursor| cursor.accounted)
            .sum::<usize>()
            + state
                .checkpoints
                .values()
                .map(|checkpoint| checkpoint.accounted)
                .sum::<usize>();
        let projections: usize = state
            .projections
            .values()
            .map(|cursor| {
                crate::snapshot_memory::value_charge(&cursor.request).owned_capacity_bytes
                    + crate::snapshot_memory::value_charge(&cursor.request)
                        .opaque_dom_accounted_bytes
            })
            .sum();
        let graph_bytes: usize = state.graphs.values().map(|graph| graph.accounted).sum();
        let delivery_bytes: usize = state
            .deliveries
            .iter()
            .map(|(key, delivery)| {
                key.capacity()
                    + size_of::<Delivery>()
                    + delivery.claimant.capacity()
                    + delivery.leases.capacity() * size_of::<String>()
                    + delivery.leases.iter().map(String::capacity).sum::<usize>()
                    + delivery.cursor.as_ref().map_or(0, String::capacity)
            })
            .sum();
        let registry_metadata = state.registries.capacity() * size_of::<(String, RegistryRecord)>()
            + state
                .registries
                .iter()
                .map(|(key, record)| {
                    key.capacity() + record.allocations.capacity() * size_of::<(usize, usize)>()
                })
                .sum::<usize>();
        let metadata = delivery_bytes
            + registry_metadata
            + state
                .generations
                .values()
                .map(|record| {
                    record.registry_generation.capacity()
                        + record.entries.capacity() * size_of::<(usize, MemoryCharge)>()
                        + record.indexes.capacity() * size_of::<(usize, usize)>()
                })
                .sum::<usize>()
            + size_of::<StoreState>()
            + state.leases.capacity() * size_of::<(String, Lease)>()
            + state.waiters.capacity() * size_of::<(String, Waiter)>()
            + state.latest.capacity() * size_of::<(SourceIdentity, Arc<TranscriptSnapshot>)>()
            + state.escaped_chunks.capacity()
                * size_of::<(usize, (Weak<Vec<Entry>>, MemoryCharge))>()
            + state.generations.capacity() * size_of::<(String, GenerationRecord)>()
            + state
                .leases
                .iter()
                .map(|(key, lease)| {
                    key.capacity()
                        + lease.claimant.capacity()
                        + lease.registry_generation.capacity()
                })
                .sum::<usize>()
            + state
                .waiters
                .iter()
                .map(|(key, waiter)| {
                    let charge = crate::snapshot_memory::value_charge(&waiter.context);
                    key.capacity()
                        + waiter.claimant.capacity()
                        + charge.owned_capacity_bytes
                        + charge.opaque_dom_accounted_bytes
                })
                .sum::<usize>()
            + state
                .resolutions
                .values()
                .map(|cursor| {
                    let charge = crate::snapshot_memory::value_charge(&cursor.request);
                    charge.owned_capacity_bytes
                        + charge.opaque_dom_accounted_bytes
                        + cursor.ids.iter().map(String::capacity).sum::<usize>()
                        + cursor
                            .paths
                            .values()
                            .map(|path| path.as_os_str().len())
                            .sum::<usize>()
                        + cursor
                            .sessions
                            .iter()
                            .map(|value| {
                                let charge = crate::snapshot_memory::value_charge(value);
                                charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
            + state.locations.capacity() * size_of::<(String, LocatedPath)>()
            + state
                .locations
                .iter()
                .map(|(id, location)| id.capacity() + location.path.as_os_str().len())
                .sum::<usize>()
            + state.locates.capacity() * size_of::<(String, LocateCursor)>()
            + state
                .locates
                .iter()
                .map(|(token, cursor)| token.capacity() + cursor.accounted_bytes())
                .sum::<usize>();
        json!({"retained_entry_capacity_bytes": entries, "retained_index_capacity_bytes": indexes,
            "retained_projection_bytes": projections + discovery_bytes + graph_bytes + metadata + state.owned.accounted_bytes(), "pending_input_capacity_bytes": pending + classifier_bytes + label_bytes + state.transient_bytes,
            "retained_total_accounted_bytes": entries + indexes + pending + classifier_bytes + label_bytes + projections + discovery_bytes + graph_bytes + metadata + state.transient_bytes + state.owned.accounted_bytes(),
            "active_leases": state.leases.len(), "pending_loads": state.loads.len(), "live_generations": snapshots.len()})
    }

    #[cfg(test)]
    pub(crate) fn retained_accounted_bytes(&self) -> usize {
        let state = self.state.lock().expect("snapshot store");
        number(&Self::gauges(&state), "retained_total_accounted_bytes").unwrap()
    }

    fn memory_cap(&self, context: &Value) -> Result<usize, SnapshotError> {
        Ok(if str_field(context, "admission")? == "hook" {
            self.config.retained
        } else {
            self.config.retained.saturating_sub(self.config.hook_bytes)
        })
    }

    fn admit_memory(
        &self,
        state: &mut StoreState,
        context: &Value,
        additional: usize,
    ) -> Result<(), SnapshotError> {
        let cap = self.memory_cap(context)?;
        if number(&Self::gauges(state), "retained_total_accounted_bytes")?
            .saturating_add(additional)
            > cap
        {
            self.classified
                .lock()
                .expect("classified snapshots")
                .retain(|_, snapshot| Arc::strong_count(snapshot) > 1);
            let leased: HashSet<_> = state
                .leases
                .values()
                .map(|lease| lease.snapshot.id.clone())
                .collect();
            state.latest.retain(|_, snapshot| {
                leased.contains(&snapshot.id) || Arc::strong_count(snapshot) > 1
            });
        }
        if number(&Self::gauges(state), "retained_total_accounted_bytes")?
            .saturating_add(additional)
            > cap
        {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "accounted storage admission exhausted",
            ));
        }
        Ok(())
    }

    fn lease_cap(&self, context: &Value) -> Result<usize, SnapshotError> {
        Ok(if str_field(context, "admission")? == "hook" {
            self.config.leases
        } else {
            self.config.leases.saturating_sub(self.config.hook_leases)
        })
    }

    fn issue(
        &self,
        state: &mut StoreState,
        snapshot: Arc<TranscriptSnapshot>,
        classifier: Value,
        context: &Value,
        absolute_deadline: u64,
    ) -> Result<Value, SnapshotError> {
        let cap = if str_field(context, "admission")? == "hook" {
            self.config.leases
        } else {
            self.config.leases.saturating_sub(self.config.hook_leases)
        };
        if state.leases.len() >= cap {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "lease admission exhausted",
            ));
        }
        let registry = state
            .registries
            .get(str_field(context, "registry_generation")?)
            .map(|record| Arc::clone(&record.snapshot))
            .ok_or_else(|| invalid("tool registry generation is not registered with this owner"))?;
        let now = now_ms();
        if now >= absolute_deadline {
            return Err(SnapshotError::new(
                Status::Deadline,
                "lease preparation deadline expired",
            ));
        }
        let expires = (now + self.config.ttl).min(absolute_deadline);
        let token = self.token("lease");
        let description = self.description(&snapshot, &classifier, &token, expires);
        state.leases.insert(
            token,
            Lease {
                claimant: str_field(context, "claimant")?.to_owned(),
                snapshot,
                classifier,
                expires,
                absolute_deadline,
                exposed: false,
                registry_generation: str_field(context, "registry_generation")?.to_owned(),
                registry,
            },
        );
        Ok(json!({"kind": "acquired", "description": description}))
    }

    fn description(
        &self,
        snapshot: &TranscriptSnapshot,
        classifier: &Value,
        lease: &str,
        expires: u64,
    ) -> Value {
        json!({"handle": {"owner_epoch": self.owner_epoch, "snapshot_id": snapshot.id, "generation": snapshot.id, "lease_id": lease},
            "canonical_path": snapshot.canonical_path.to_string_lossy().as_ref(),
            "source_id": format!("{}:{}", snapshot.stamp.identity.device, snapshot.stamp.identity.inode),
            "device": snapshot.stamp.identity.device.to_string(), "inode": snapshot.stamp.identity.inode.to_string(),
            "mtime_ns": snapshot.stamp.mtime_ns.to_string(), "ctime_ns": snapshot.stamp.ctime_ns.to_string(),
            "provider": snapshot.provider.as_str(), "parser_version": PARSER_VERSION,
            "source_bytes": snapshot.stamp.size, "committed_bytes": snapshot.committed_bytes,
            "event_count": snapshot.event_count, "turn_count": snapshot.activity.turn_count(),
            "classifier": classifier, "provisional_tail": snapshot.provisional_tail,"lease_expires_unix_ms":expires})
    }

    pub fn pin(
        &self,
        handle: &Value,
        context: &Value,
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        self.pin_scope(handle, context)
            .map(|(snapshot, _)| snapshot)
    }

    pub fn validate_scope(&self, handle: &Value, context: &Value) -> Result<(), SnapshotError> {
        let state = self.state.lock().expect("snapshot state");
        self.lease(&state, handle, context).map(|_| ())
    }

    pub fn pin_scope(
        &self,
        handle: &Value,
        context: &Value,
    ) -> Result<(Arc<TranscriptSnapshot>, Value), SnapshotError> {
        self.authority(context, None)?;
        let (snapshot, description) = {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            let lease = self.lease(&state, handle, context)?;
            (
                Arc::clone(&lease.snapshot),
                self.description(
                    &lease.snapshot,
                    &lease.classifier,
                    str_field(handle, "lease_id")?,
                    lease.expires,
                ),
            )
        };
        self.authority(context, Some(&snapshot.canonical_path))?;
        Ok((snapshot, description))
    }

    pub fn pin_scope_for_work(
        &self,
        handle: &Value,
        context: &Value,
        deadline: u64,
    ) -> Result<(Arc<TranscriptSnapshot>, Value), SnapshotError> {
        if deadline <= now_ms() {
            return Err(SnapshotError::new(
                Status::Deadline,
                "borrow deadline expired",
            ));
        }
        let (snapshot, _) = self.pin_scope(handle, context)?;
        let mut state = self.state.lock().expect("snapshot state");
        let lease = self.lease(&state, handle, context)?;
        let expires = lease.expires.max(deadline.min(lease.absolute_deadline));
        let classifier = lease.classifier.clone();
        let token = str_field(handle, "lease_id")?;
        state
            .leases
            .get_mut(token)
            .expect("validated lease")
            .expires = expires;
        let description = self.description(&snapshot, &classifier, token, expires);
        Ok((snapshot, description))
    }

    fn lease<'a>(
        &self,
        state: &'a StoreState,
        handle: &Value,
        context: &Value,
    ) -> Result<&'a Lease, SnapshotError> {
        let stale = || {
            SnapshotError::new(
                Status::StaleHandle,
                "lease does not belong to this claimant or generation",
            )
        };
        if str_field(handle, "owner_epoch")? != self.owner_epoch {
            return Err(stale());
        }
        let lease = state
            .leases
            .get(str_field(handle, "lease_id")?)
            .ok_or_else(stale)?;
        if lease.claimant != str_field(context, "claimant")?
            || lease.registry_generation != str_field(context, "registry_generation")?
            || lease.snapshot.id != str_field(handle, "snapshot_id")?
            || lease.snapshot.id != str_field(handle, "generation")?
            || lease.expires <= now_ms()
        {
            return Err(stale());
        }
        Ok(lease)
    }

    pub fn request(&self, request: &Value, context: &Value, cancel: &Cancellation) -> Value {
        let mut usage = [0u64; 18];
        let id = request.get("id").cloned().unwrap_or(json!("invalid"));
        let _reply_reservation = if request.get("operation").and_then(Value::as_str)
            == Some("release")
        {
            None
        } else {
            match self.reserve_projection(context, MAX_REPLY_BYTES.saturating_mul(2)) {
                Ok(reservation) => Some(reservation),
                Err(error) => {
                    usage[12] = 1;
                    self.state.lock().expect("snapshot state").counters[12] += 1;
                    return json!({"schema":SCHEMA,"id":id,"status":error.status.as_str(),"complete":false,"data":null,"cursor":null,"reason":error.reason,"usage":usage_value(&usage)});
                }
            }
        };
        let output_limit = request
            .get("limits")
            .and_then(|value| value.get("max_output_bytes"))
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .or_else(|| {
                request
                    .get("cursor")
                    .and_then(Value::as_str)
                    .and_then(|token| {
                        let state = self.state.lock().expect("snapshot state");
                        state
                            .waiters
                            .get(token)
                            .map(|waiter| waiter.limits.max_output_bytes)
                            .or_else(|| {
                                state
                                    .projections
                                    .get(token)
                                    .map(|cursor| cursor.limits.max_output_bytes)
                            })
                            .or_else(|| {
                                state.discoveries.get(token).map(|cursor| {
                                    cursor
                                        .limits
                                        .max_output_bytes
                                        .saturating_sub(cursor.output_bytes)
                                })
                            })
                            .or_else(|| {
                                state
                                    .resolutions
                                    .get(token)
                                    .map(|cursor| cursor.remaining.max_output_bytes)
                            })
                            .or_else(|| {
                                state.locates.get(token).map(|cursor| {
                                    cursor
                                        .limits
                                        .max_output_bytes
                                        .saturating_sub(cursor.output_bytes)
                                })
                            })
                            .or_else(|| {
                                state
                                    .graphs
                                    .get(token)
                                    .map(|graph| graph.remaining.max_output_bytes)
                            })
                    })
            })
            .unwrap_or(self.config.output)
            .min(self.config.output)
            .min(MAX_DATA_BYTES);
        let dispatched = if matches!(
            request.get("operation").and_then(Value::as_str),
            Some("release" | "stats")
        ) {
            self.dispatch(request, context, cancel, &mut usage)
        } else {
            self.registry(context).and_then(|registry| {
                crate::toolcall::with_registry(registry, || {
                    self.dispatch(request, context, cancel, &mut usage)
                })
            })
        };
        let outcome = dispatched.and_then(|(mut data, cursor, reason)| {
            let bytes=match encoded_size(&data,output_limit) {
                Ok(bytes)=>bytes,
                Err(error)=> {
                    let pending=json!({"id":id,"status":if reason.is_some(){"incomplete"}else{"ok"},"data":data,"cursor":cursor});
                    self.track_delivery(&pending,context,request.get("operation").and_then(Value::as_str)!=Some("describe"));
                    self.discard_response(&pending,context)?;
                    return Err(error);
                }
            };
            if bytes > output_limit {
                if let Some(token) = &cursor {
                    self.detach(&json!({"cursor":token}), context);
                }
                if data.get("kind").and_then(Value::as_str) == Some("acquired")
                    && request.get("operation").and_then(Value::as_str) != Some("describe")
                {
                    if let Some(token) = data
                        .get("description")
                        .and_then(|value| value.get("handle"))
                        .and_then(|value| value.get("lease_id"))
                        .and_then(Value::as_str)
                    {
                        self.state
                            .lock()
                            .expect("snapshot state")
                            .leases
                            .remove(token);
                    }
                }
                return Err(SnapshotError::new(
                    Status::OutputLimit,
                    "response data exceeds output budget",
                ));
            }
            if data.get("kind").and_then(Value::as_str) == Some("loading") {
                if let Some(token) = &cursor {
                    let mut state = self.state.lock().expect("snapshot state");
                    if let Some(waiter) = state.waiters.get_mut(token) {
                        if bytes > waiter.limits.max_output_bytes {
                            return Err(SnapshotError::new(
                                Status::OutputLimit,
                                "reservation output budget exhausted",
                            ));
                        }
                        waiter.limits.max_output_bytes -= bytes;
                        data["reservation"]["remaining_work"]
                            .insert("max_output_bytes", json!(waiter.limits.max_output_bytes));
                    }
                }
            }
            Ok((data, cursor, reason))
        });
        let mut response = match outcome {
            Ok((data, cursor, reason)) => json!({"schema": SCHEMA, "id": id,
                "status": if reason.is_some() { "incomplete" } else { "ok" }, "complete": reason.is_none(),
                "data": data, "cursor": cursor, "reason": reason, "usage": usage_value(&usage)}),
            Err(error) => {
                if matches!(
                    error.status,
                    Status::Cancelled | Status::Deadline | Status::OutputLimit
                ) {
                    self.detach(request, context);
                }
                usage[if error.status == Status::Cancelled {
                    11
                } else {
                    12
                }] += 1;
                json!({"schema": SCHEMA, "id": id, "status": error.status.as_str(), "complete": false,
                    "data": null, "cursor": null, "reason": error.reason, "usage": usage_value(&usage)})
            }
        };
        for _ in 0..3 {
            match encoded_size(&response, MAX_REPLY_BYTES) {
                Ok(bytes) => {
                    usage[13] = bytes as u64;
                }
                Err(error) => {
                    self.detach(request, context);
                    usage[12] += 1;
                    response = json!({"schema":SCHEMA,"id":id,"status":error.status.as_str(),"complete":false,"data":null,"cursor":null,"reason":error.reason,"usage":usage_value(&usage)});
                }
            }
            response.insert("usage", usage_value(&usage));
        }
        self.track_delivery(
            &response,
            context,
            request.get("operation").and_then(Value::as_str) != Some("describe"),
        );
        let mut state = self.state.lock().expect("snapshot state");
        for (total, own) in state.counters.iter_mut().zip(usage.iter()) {
            *total += own;
        }
        response
    }

    fn response_handles(response: &Value) -> Result<Vec<String>, SnapshotError> {
        let mut result = Vec::new();
        let data = response.get("data").unwrap_or(response);
        let mut add = |description: &Value| {
            if let Some(token) = description
                .get("handle")
                .and_then(|handle| handle.get("lease_id"))
                .and_then(Value::as_str)
            {
                result.push(token.to_owned());
            }
        };
        if data.get("kind").is_none() && data.get("description").is_some() {
            add(&data["description"]);
        }
        match data.get("kind").and_then(Value::as_str) {
            Some("acquired") => add(&data["description"]),
            Some("resolved") => {
                if let Some(sessions) = data["sessions"].as_array() {
                    for session in sessions.iter() {
                        add(&session["description"]);
                    }
                }
            }
            Some("records")
                if data["record_schema"].as_str() == Some("cc-transcript.sidechain/1") =>
            {
                if let Some(records) = data["records_json"].as_array() {
                    for record in records.iter() {
                        let value: Value = sonic_rs::from_str(
                            record
                                .as_str()
                                .ok_or_else(|| invalid("invalid sidechain record"))?,
                        )
                        .map_err(|error| invalid(error.to_string()))?;
                        add(&value["description"]);
                    }
                }
            }
            _ => {}
        }
        Ok(result)
    }

    fn response_cursor(response: &Value) -> Option<&str> {
        response.get("cursor").and_then(Value::as_str).or_else(|| {
            response
                .get("data")
                .and_then(|data| data.get("cursor"))
                .and_then(Value::as_str)
        })
    }

    fn delivery_key(
        &self,
        response: &Value,
        context: &Value,
        handles: &[String],
    ) -> Result<String, SnapshotError> {
        let cursor = Self::response_cursor(response).unwrap_or("");
        let helper_cursor =
            cursor.starts_with("domain-projection:") || cursor.starts_with("classifier:");
        let data = response.get("data").unwrap_or(response);
        let label_result = data
            .get("description")
            .and_then(|description| description.get("handle"))
            .and_then(|handle| handle.get("snapshot_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("labels:"));
        let helper = helper_cursor || label_result;
        let mut digest = Sha256::new();
        digest.update(self.seed);
        for value in [
            str_field(context, "claimant")?,
            if helper {
                ""
            } else {
                response.get("id").and_then(Value::as_str).unwrap_or("")
            },
            if helper {
                ""
            } else {
                response.get("status").and_then(Value::as_str).unwrap_or("")
            },
            cursor,
        ] {
            digest.update(value.len().to_le_bytes());
            digest.update(value.as_bytes());
        }
        if !helper_cursor {
            for handle in handles {
                digest.update(handle.len().to_le_bytes());
                digest.update(handle.as_bytes());
            }
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    pub(crate) fn track_delivery(&self, response: &Value, context: &Value, may_issue: bool) {
        let Ok(handles) = Self::response_handles(response) else {
            return;
        };
        let Ok(key) = self.delivery_key(response, context, &handles) else {
            return;
        };
        let Ok(claimant) = str_field(context, "claimant") else {
            return;
        };
        let cursor = Self::response_cursor(response).map(str::to_owned);
        let mut state = self.state.lock().expect("snapshot state");
        let mut leases = Vec::new();
        if may_issue {
            for handle in handles {
                if let Some(lease) = state.leases.get_mut(&handle) {
                    if lease.claimant == claimant && !lease.exposed {
                        lease.exposed = true;
                        leases.push(handle);
                    }
                }
            }
        }
        if leases.is_empty() && cursor.is_none() {
            state.deliveries.remove(&key);
            return;
        }
        if state.deliveries.len() >= self.config.leases.saturating_mul(4) {
            if let Some(oldest) = state
                .deliveries
                .iter()
                .min_by_key(|(_, delivery)| delivery.expires)
                .map(|(token, _)| token.clone())
            {
                state.deliveries.remove(&oldest);
            }
        }
        state.deliveries.insert(
            key,
            Delivery {
                claimant: claimant.to_owned(),
                leases,
                cursor,
                expires: now_ms() + self.config.ttl,
            },
        );
    }

    pub fn discard_response(
        &self,
        response: &Value,
        context: &Value,
    ) -> Result<bool, SnapshotError> {
        self.authority(context, None)?;
        let handles = Self::response_handles(response)?;
        let key = self.delivery_key(response, context, &handles)?;
        let delivery = {
            let mut state = self.state.lock().expect("snapshot state");
            state.deliveries.remove(&key)
        };
        let Some(delivery) = delivery else {
            return Ok(false);
        };
        if delivery.claimant != str_field(context, "claimant")? {
            return Err(SnapshotError::new(
                Status::PermissionDenied,
                "response claimant differs",
            ));
        }
        if let Some(cursor) = delivery.cursor {
            match self.release_cursor(&cursor, context) {
                Ok(_) => {}
                Err(error) if error.status == Status::StaleCursor => {}
                Err(error) => return Err(error),
            }
        }
        let mut state = self.state.lock().expect("snapshot state");
        for token in delivery.leases {
            if state
                .leases
                .get(&token)
                .is_some_and(|lease| lease.claimant == delivery.claimant)
            {
                state.leases.remove(&token);
                state.owned.release_lease(&token);
            }
        }
        Self::prune(&mut state);
        Ok(true)
    }

    fn release_cursor(&self, token: &str, context: &Value) -> Result<bool, SnapshotError> {
        let claimant = str_field(context, "claimant")?;
        let mut state = self.state.lock().expect("snapshot state");
        let foreign = state
            .labels
            .get(token)
            .is_some_and(|slot| slot.preparation.binding().claimant != claimant)
            || state
                .graphs
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .projections
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .discoveries
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .resolutions
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .locates
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .checkpoints
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .waiters
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant);
        if foreign {
            return Err(SnapshotError::new(
                Status::StaleCursor,
                "cursor claimant differs",
            ));
        }
        let mut released = state.labels.remove(token).is_some();
        released |= state.projections.remove(token).is_some();
        released |= state.discoveries.remove(token).is_some();
        released |= state.checkpoints.remove(token).is_some();
        released |= state.waiters.remove(token).is_some();
        if let Some(graph) = state.graphs.remove(token) {
            Self::release_graph_state(&mut state, &graph);
            released = true;
        }
        if let Some(resolution) = state.resolutions.remove(token) {
            if let Some(pending) = resolution.pending {
                state.waiters.remove(&pending);
            }
            released = true;
        }
        released |= state.locates.remove(token).is_some();
        released |= state.owned.discard_cursor(token, claimant)?;
        Self::prune(&mut state);
        Ok(released)
    }

    fn detach(&self, request: &Value, context: &Value) {
        let Some(token) = request.get("cursor").and_then(Value::as_str) else {
            return;
        };
        let Some(claimant) = context.get("claimant").and_then(Value::as_str) else {
            return;
        };
        let mut state = self.state.lock().expect("snapshot state");
        if state
            .graphs
            .get(token)
            .is_some_and(|graph| graph.claimant == claimant)
        {
            if let Some(graph) = state.graphs.remove(token) {
                Self::release_graph_state(&mut state, &graph);
            }
        }
        if state
            .waiters
            .get(token)
            .is_some_and(|waiter| waiter.claimant == claimant)
        {
            state.waiters.remove(token);
        }
        if state
            .projections
            .get(token)
            .is_some_and(|cursor| cursor.claimant == claimant)
        {
            state.projections.remove(token);
        }
        if state
            .discoveries
            .get(token)
            .is_some_and(|cursor| cursor.claimant == claimant)
        {
            state.discoveries.remove(token);
        }
        if state
            .resolutions
            .get(token)
            .is_some_and(|cursor| cursor.claimant == claimant)
        {
            if let Some(cursor) = state.resolutions.remove(token) {
                if let Some(pending) = cursor.pending {
                    state.waiters.remove(&pending);
                }
            }
        }
        if state
            .locates
            .get(token)
            .is_some_and(|cursor| cursor.claimant == claimant)
        {
            state.locates.remove(token);
        }
        Self::prune(&mut state);
    }

    fn dispatch(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        self.authority(context, None)?;
        if str_field(request, "schema")? != SCHEMA {
            return Err(invalid("unsupported snapshot schema"));
        }
        let operation = str_field(request, "operation")?;
        if operation != "resume" {
            cancel.check(u64::MAX)?;
        }
        match operation {
            "acquire" => self.acquire(request, context, cancel, usage),
            "resume" => {
                let cursor = str_field(request, "cursor")?;
                let registry = str_field(context, "registry_generation")?;
                let admission = str_field(context, "admission")?;
                {
                    let state = self.state.lock().expect("snapshot state");
                    let differs = state.waiters.get(cursor).is_some_and(|waiter| {
                        waiter.context["registry_generation"].as_str() != Some(registry)
                            || waiter.context["admission"].as_str() != Some(admission)
                    }) || state.graphs.get(cursor).is_some_and(|graph| {
                        graph.context["registry_generation"].as_str() != Some(registry)
                            || graph.context["admission"].as_str() != Some(admission)
                    }) || state.discoveries.get(cursor).is_some_and(|scan| {
                        scan.context["registry_generation"].as_str() != Some(registry)
                            || scan.context["admission"].as_str() != Some(admission)
                    }) || state.resolutions.get(cursor).is_some_and(|scan| {
                        scan.context["registry_generation"].as_str() != Some(registry)
                            || scan.context["admission"].as_str() != Some(admission)
                    }) || state.locates.get(cursor).is_some_and(|scan| {
                        scan.context["registry_generation"].as_str() != Some(registry)
                            || scan.context["admission"].as_str() != Some(admission)
                    }) || state.projections.get(cursor).is_some_and(|projection| {
                        projection.registry_generation != registry
                            || projection.admission != admission
                    });
                    if differs {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor registry or admission differs",
                        ));
                    }
                }
                cancel.check(u64::MAX)?;
                let (waiter, projection) = {
                    let mut state = self.state.lock().expect("snapshot state");
                    Self::prune(&mut state);
                    (
                        state.waiters.get(cursor).cloned(),
                        state.projections.get(cursor).cloned(),
                    )
                };
                let discovery = {
                    let mut state = self.state.lock().expect("snapshot state");
                    if state.discoveries.get(cursor).is_some_and(|scan| {
                        scan.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    state.discoveries.remove(cursor)
                };
                if let Some(mut discovery) = discovery {
                    discovery.context = context.clone();
                    return self.scan(cursor, discovery, cancel, usage);
                }
                let resolution = {
                    let mut state = self.state.lock().expect("snapshot state");
                    if state.resolutions.get(cursor).is_some_and(|scan| {
                        scan.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    state.resolutions.remove(cursor)
                };
                if let Some(mut resolution) = resolution {
                    resolution.context = context.clone();
                    return self.resolve_step(cursor, resolution, cancel, usage);
                }
                let locate = {
                    let mut state = self.state.lock().expect("snapshot state");
                    if state.locates.get(cursor).is_some_and(|scan| {
                        scan.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "location cursor claimant differs",
                        ));
                    }
                    state.locates.remove(cursor)
                };
                if let Some(mut locate) = locate {
                    locate.context = context.clone();
                    return self.locate_step(cursor, locate, cancel, usage);
                }
                let graph = {
                    let mut state = self.state.lock().expect("snapshot state");
                    if state.graphs.get(cursor).is_some_and(|graph| {
                        graph.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "graph cursor claimant differs",
                        ));
                    }
                    state.graphs.remove(cursor)
                };
                if let Some(mut graph) = graph {
                    graph.context = context.clone();
                    return self.graph_step(cursor, graph, cancel, usage);
                }
                if let Some(mut waiter) = waiter {
                    if waiter.claimant != str_field(context, "claimant")? {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    self.authority(context, Some(&waiter.load.path))?;
                    waiter.context = context.clone();
                    if let Some(current) = self
                        .state
                        .lock()
                        .expect("snapshot state")
                        .waiters
                        .get_mut(cursor)
                    {
                        current.context = context.clone();
                    }
                    return self.advance(cursor, waiter, cancel, usage);
                }
                if let Some(projection) = projection {
                    if projection.claimant != str_field(context, "claimant")? {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    self.state
                        .lock()
                        .expect("snapshot state")
                        .projections
                        .remove(cursor);
                    return self.project_request(
                        &projection.request,
                        context,
                        cancel,
                        projection.limits,
                        projection.next,
                    );
                }
                Err(SnapshotError::new(
                    Status::StaleCursor,
                    "cursor expired or belongs to another owner",
                ))
            }
            "describe" | "retain" | "renew" => {
                let handle = request
                    .get("handle")
                    .ok_or_else(|| invalid("missing handle"))?;
                let snapshot = self.pin(handle, context)?;
                let mut state = self.state.lock().expect("snapshot state");
                let lease = self.lease(&state, handle, context)?;
                let classifier = lease.classifier.clone();
                let absolute_deadline = lease.absolute_deadline;
                let current_expiry = lease.expires;
                match operation {
                    "retain" => Ok((
                        self.issue(&mut state, snapshot, classifier, context, absolute_deadline)?,
                        None,
                        None,
                    )),
                    "renew" => {
                        let expires = (now_ms() + self.config.ttl)
                            .min(absolute_deadline)
                            .max(current_expiry);
                        state
                            .leases
                            .get_mut(str_field(handle, "lease_id")?)
                            .expect("validated lease")
                            .expires = expires;
                        Ok((
                            json!({"kind": "renewed", "expires_unix_ms": expires}),
                            None,
                            None,
                        ))
                    }
                    _ => Ok((
                        json!({"kind": "acquired", "description": self.description(&snapshot, &classifier, str_field(handle, "lease_id")?,current_expiry)}),
                        None,
                        None,
                    )),
                }
            }
            "release" => {
                str_field(context, "admission")?;
                let kind = str_field(request, "kind")?;
                if kind != "cursor"
                    || request
                        .get("owner_epoch")
                        .is_some_and(|epoch| !epoch.is_null())
                {
                    if str_field(request, "owner_epoch")? != self.owner_epoch {
                        return Err(SnapshotError::new(
                            Status::StaleHandle,
                            "owner epoch differs",
                        ));
                    }
                }
                let token = str_field(request, "token")?;
                if str_field(request, "kind")? == "cursor" {
                    return Ok((
                        json!({"kind":"released","released":self.release_cursor(token,context)?}),
                        None,
                        None,
                    ));
                }
                let claimant = str_field(context, "claimant")?;
                let mut state = self.state.lock().expect("snapshot state");
                let released = match str_field(request, "kind")? {
                    "lease" => {
                        if state
                            .leases
                            .get(token)
                            .is_some_and(|lease| lease.claimant != claimant)
                        {
                            return Err(SnapshotError::new(
                                Status::StaleHandle,
                                "lease claimant differs",
                            ));
                        }
                        let released = state.leases.remove(token).is_some();
                        if released {
                            state.owned.release_lease(token);
                            state.labels.retain(|_, slot| {
                                slot.source_handle["lease_id"].as_str() != Some(token)
                            });
                        }
                        released
                    }
                    "reservation" => {
                        if state
                            .waiters
                            .get(token)
                            .is_some_and(|waiter| waiter.claimant != claimant)
                        {
                            return Err(SnapshotError::new(
                                Status::StaleCursor,
                                "reservation claimant differs",
                            ));
                        }
                        state.waiters.remove(token).is_some()
                    }
                    _ => return Err(invalid("unknown release kind")),
                };
                Self::prune(&mut state);
                Ok((
                    json!({"kind": "released", "released": released}),
                    None,
                    None,
                ))
            }
            "stats" => {
                let mut state = self.state.lock().expect("snapshot state");
                Self::prune(&mut state);
                Ok((
                    json!({"kind": "stats", "counters": usage_value(&state.counters), "gauges": Self::gauges(&state)}),
                    None,
                    None,
                ))
            }
            "query" if Self::is_graph_request(request) => {
                self.graph(request, context, cancel, usage)
            }
            "query" | "capture" | "activity_probe" | "hydrate" | "mine" => {
                self.project_request(request, context, cancel, limits(request)?, 0)
            }
            "discover" | "resolve" => self.discover(request, context, cancel, usage),
            "locate" => self.locate(request, context, cancel, usage),
            _ => Err(invalid("unsupported operation")),
        }
    }

    fn acquire(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let limits = limits(request)?;
        let registry = self.registry(context)?;
        cancel.check(limits.deadline_unix_ms)?;
        let classifier = request
            .get("classifier")
            .ok_or_else(|| invalid("missing classifier"))?;
        let classifier_id = str_field(classifier, "id")?;
        let classifier_version = str_field(classifier, "version")?;
        let classifier_key = sonic_rs::to_string(&json!([classifier_id, classifier_version]))
            .expect("classifier key");
        if !(classifier_id == "native" && classifier_version == "1")
            && !self
                .classifiers
                .lock()
                .expect("classifiers")
                .contains_key(&classifier_key)
        {
            return Err(invalid(
                "classifier version is not registered with this owner",
            ));
        }
        let path = std::fs::canonicalize(str_field(request, "path")?).map_err(io_error)?;
        self.authority(context, Some(&path))?;
        usage[0] += 1;
        let file = File::open(&path).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("source must be a regular file"));
        }
        let stamp = SourceStamp::of(&metadata);
        if stamp.size > self.config.source as u64 {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "source exceeds owner bound",
            ));
        }
        if !Self::matches_prefix(
            SourceStamp::of(&std::fs::metadata(&path).map_err(io_error)?),
            stamp,
        ) {
            return Err(SnapshotError::new(
                Status::Changed,
                "source replaced during open",
            ));
        }
        let classifier = request
            .get("classifier")
            .ok_or_else(|| invalid("missing classifier"))?
            .clone();
        let now = now_ms();
        let cursor = self.token("reservation");
        let waiter = {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            let cached = state
                .latest
                .get(&stamp.identity)
                .filter(|snapshot| snapshot.stamp == stamp)
                .cloned();
            if cached.is_some() {
                usage[7] += 1;
            }
            let slot = if let Some(slot) = state.loads.get(&stamp.identity) {
                if slot.stamp != stamp {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "source changed during shared preparation",
                    ));
                }
                usage[8] += 1;
                Arc::clone(slot)
            } else {
                let cap = if str_field(context, "admission")? == "hook" {
                    self.config.loads
                } else {
                    self.config.loads.saturating_sub(self.config.hook_loads)
                };
                if state.loads.len() >= cap {
                    return Err(SnapshotError::new(
                        Status::RetainedLimit,
                        "preparation admission exhausted",
                    ));
                }
                self.admit_memory(
                    &mut state,
                    context,
                    if cached.is_some() {
                        size_of::<LoadSlot>()
                    } else {
                        self.config.read_step.min(stamp.size as usize)
                    },
                )?;
                let previous = state
                    .latest
                    .get(&stamp.identity)
                    .filter(|old| {
                        old.provider == Provider::Claude
                            && old.event_count > 0
                            && old.stamp.size < stamp.size
                    })
                    .cloned();
                let previous_index_compatible = previous.as_ref().is_some_and(|previous| {
                    state
                        .generations
                        .get(&previous.id)
                        .is_some_and(|generation| {
                            generation.registry_generation
                                == str_field(context, "registry_generation").unwrap_or("")
                        })
                });
                if cached.is_none() {
                    usage[if previous.is_some() { 5 } else { 4 }] += 1;
                }
                let slot = Arc::new(LoadSlot {
                    id: self.token("load"),
                    path: path.clone(),
                    stamp,
                    registry_generation: str_field(context, "registry_generation")?.to_owned(),
                    registry,
                    accounted: AtomicUsize::new(0),
                    deadline: now + self.config.preparation,
                    work: Mutex::new(Load {
                        file,
                        path,
                        offset: 0,
                        pending: Vec::new(),
                        pending_start: 0,
                        provider: None,
                        chunks: Vec::new(),
                        count: 0,
                        activity: ActivityIndex::default(),
                        indexed: 0,
                        decoded: false,
                        session_id: None,
                        origin_fence: Vec::new(),
                        origin_complete: false,
                        seal_fence: Vec::new(),
                        sealed: false,
                        prefix_fence: Vec::new(),
                        previous,
                        previous_index_compatible,
                        prefix_checked: false,
                        fence: Vec::new(),
                        committed: 0,
                        provisional: false,
                        result: cached,
                        failure: None,
                    }),
                });
                state.loads.insert(stamp.identity, Arc::clone(&slot));
                slot
            };
            if state.waiters.len() >= self.lease_cap(context)? {
                return Err(SnapshotError::new(
                    Status::LeaseLimit,
                    "reservation admission exhausted",
                ));
            }
            let deadline = limits.deadline_unix_ms.min(slot.deadline);
            let waiter = Waiter {
                claimant: str_field(context, "claimant")?.to_owned(),
                load: slot,
                classifier,
                context: context.clone(),
                limits,
                created: now,
                expires: (now + self.config.ttl).min(deadline),
                deadline,
                used_bytes: 0,
                used_events: 0,
                busy: false,
            };
            state.waiters.insert(cursor.clone(), waiter.clone());
            waiter
        };
        self.advance(&cursor, waiter, cancel, usage)
    }

    fn loading(
        &self,
        token: &str,
        waiter: &Waiter,
        reason: &str,
    ) -> (Value, Option<String>, Option<String>) {
        (
            json!({"kind": "loading", "reservation": {"owner_epoch": self.owner_epoch, "reservation_id": token,
            "load_id": waiter.load.id, "created_unix_ms": waiter.created, "expires_unix_ms": waiter.expires,
            "absolute_deadline_unix_ms": waiter.deadline, "remaining_work": {
                "max_read_bytes": waiter.limits.max_read_bytes.saturating_sub(waiter.used_bytes),
                "max_events": waiter.limits.max_events.saturating_sub(waiter.used_events),
                "max_items": waiter.limits.max_items, "max_output_bytes": waiter.limits.max_output_bytes,
                "max_discovery_entries": waiter.limits.max_discovery_entries, "max_sources": waiter.limits.max_sources}}}),
            Some(token.to_owned()),
            Some(reason.to_owned()),
        )
    }

    fn advance(
        &self,
        token: &str,
        mut waiter: Waiter,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        if let Err(error) = cancel.check(waiter.deadline) {
            self.state
                .lock()
                .expect("snapshot state")
                .waiters
                .remove(token);
            return Err(error);
        }
        {
            let mut state = self.state.lock().expect("snapshot state");
            let current = state
                .waiters
                .get_mut(token)
                .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "reservation released"))?;
            if current.busy {
                return Ok(self.loading(token, current, "reservation preparation is running"));
            }
            waiter = current.clone();
            current.busy = true;
        }
        let mut claim = WaiterClaim {
            store: self,
            token,
            keep: false,
        };
        let slot = Arc::clone(&waiter.load);
        let Ok(mut load) = slot.work.try_lock() else {
            claim.keep = true;
            return Ok(self.loading(token, &waiter, "shared preparation is running"));
        };
        if let Some(error) = &load.failure {
            return Err(SnapshotError::new(error.status, &error.reason));
        }
        if load.result.is_none() {
            let read_bound = self.config.read_step.min(
                waiter
                    .limits
                    .max_read_bytes
                    .saturating_sub(waiter.used_bytes),
            );
            let events_bound = self
                .config
                .event_step
                .min(waiter.limits.max_events.saturating_sub(waiter.used_events));
            if read_bound == 0 && load.offset < slot.stamp.size
                || events_bound == 0
                    && (load.offset < slot.stamp.size
                        || !load.pending.is_empty()
                        || load.indexed < load.count)
            {
                self.state
                    .lock()
                    .expect("snapshot state")
                    .waiters
                    .remove(token);
                return Err(SnapshotError::new(
                    Status::SourceLimit,
                    "cumulative preparation work budget exhausted",
                ));
            }
            let decode_source = if load.provider == Some(Provider::Codex) {
                slot.stamp.size as usize
            } else {
                self.config.entry.min(slot.stamp.size as usize)
            };
            let reservation = read_bound.saturating_add(decode_source.saturating_mul(4));
            {
                let mut state = self.state.lock().expect("snapshot state");
                self.admit_memory(&mut state, &waiter.context, reservation)?;
                slot.accounted.fetch_add(reservation, Ordering::AcqRel);
            }

            let before_bytes = usage[1];
            let before_events = usage[3];
            let before_indexed = load.indexed;
            let checked_prefix = load.prefix_checked;
            let result = crate::toolcall::with_registry(Arc::clone(&slot.registry), || {
                self.step(
                    &slot,
                    &mut load,
                    read_bound,
                    events_bound,
                    waiter.limits.max_events.saturating_sub(waiter.used_events),
                    cancel,
                    waiter.deadline,
                    usage,
                )
            });
            waiter.used_bytes += (usage[1] - before_bytes) as usize;
            waiter.used_events += (usage[3] - before_events) as usize
                + if checked_prefix == load.prefix_checked {
                    load.indexed.saturating_sub(before_indexed)
                } else {
                    0
                };
            let generation = load
                .result
                .as_ref()
                .map(|snapshot| GenerationRecord::new(snapshot, &slot.registry_generation));
            let pending_charge = if generation.is_some() {
                0
            } else {
                let shared: HashSet<_> = load
                    .previous
                    .as_ref()
                    .into_iter()
                    .flat_map(|snapshot| {
                        snapshot
                            .chunks
                            .iter()
                            .map(|chunk| Arc::as_ptr(&chunk.entries) as usize)
                    })
                    .collect();
                let charge = load.pending.capacity()
                    + load.origin_fence.capacity()
                    + load.seal_fence.capacity()
                    + load.prefix_fence.capacity()
                    + load
                        .chunks
                        .iter()
                        .filter(|chunk| !shared.contains(&(Arc::as_ptr(&chunk.entries) as usize)))
                        .map(|chunk| {
                            chunk.charge.owned_capacity_bytes
                                + chunk.charge.opaque_dom_accounted_bytes
                        })
                        .sum::<usize>();
                let prior_indexes: HashSet<_> = load
                    .previous
                    .as_ref()
                    .into_iter()
                    .flat_map(|snapshot| {
                        snapshot
                            .activity
                            .accounted_allocations()
                            .into_iter()
                            .map(|(id, _)| id)
                    })
                    .collect();
                let index_charge: usize = load
                    .activity
                    .accounted_allocations()
                    .into_iter()
                    .filter(|(id, _)| !prior_indexes.contains(id))
                    .map(|(_, bytes)| bytes)
                    .sum();
                charge + index_charge
            };
            {
                let mut state = self.state.lock().expect("snapshot state");
                if let (Some(snapshot), Some(generation)) = (&load.result, generation) {
                    state.generations.insert(snapshot.id.clone(), generation);
                }
                slot.accounted.store(pending_charge, Ordering::Release);
                self.admit_memory(&mut state, &waiter.context, 0)?;
            }
            if let Err(error) = result {
                if error.status != Status::Cancelled && error.status != Status::Deadline {
                    load.failure = Some(SnapshotError::new(error.status, &error.reason));
                }
                self.state
                    .lock()
                    .expect("snapshot state")
                    .waiters
                    .remove(token);
                return Err(error);
            }
        }
        if let Err(error) = cancel.check(waiter.deadline) {
            self.state
                .lock()
                .expect("snapshot state")
                .waiters
                .remove(token);
            return Err(error);
        }
        if let Some(snapshot) = &load.result {
            let snapshot = Arc::clone(snapshot);
            drop(load);
            let mut state = self.state.lock().expect("snapshot state");
            if !state
                .latest
                .get(&slot.stamp.identity)
                .is_some_and(|old| old.id == snapshot.id)
            {
                if state
                    .latest
                    .insert(slot.stamp.identity, Arc::clone(&snapshot))
                    .is_some()
                {
                    usage[10] += 1;
                }
                for chunk in &snapshot.chunks {
                    state.escaped_chunks.insert(
                        Arc::as_ptr(&chunk.entries) as usize,
                        (Arc::downgrade(&chunk.entries), chunk.charge),
                    );
                }
                usage[9] += 1;
            }
            drop(state);
            let mut classifier_bounds = waiter.limits;
            classifier_bounds.max_read_bytes = classifier_bounds
                .max_read_bytes
                .saturating_sub(waiter.used_bytes);
            classifier_bounds.max_events = classifier_bounds
                .max_events
                .saturating_sub(waiter.used_events);
            classifier_bounds.deadline_unix_ms = waiter.deadline;
            let classified = self.classify(
                snapshot,
                &waiter.classifier,
                &waiter.context,
                cancel,
                &classifier_bounds,
                usage,
            )?;
            waiter.used_bytes += classified.read_bytes;
            waiter.used_events += classified.events;
            let Some(snapshot) = classified.snapshot else {
                waiter.expires = (now_ms() + self.config.ttl).min(waiter.deadline);
                waiter.busy = false;
                let mut state = self.state.lock().expect("snapshot state");
                if !state.waiters.contains_key(token) {
                    return Err(SnapshotError::new(
                        Status::Cancelled,
                        "reservation released during classification",
                    ));
                }
                state.waiters.insert(token.to_owned(), waiter.clone());
                drop(state);
                claim.keep = true;
                return Ok(self.loading(token, &waiter, "classifier preparation incomplete"));
            };
            let mut state = self.state.lock().expect("snapshot state");
            if !state.waiters.contains_key(token) {
                return Err(SnapshotError::new(
                    Status::Cancelled,
                    "reservation released during preparation",
                ));
            }
            let data = self.issue(
                &mut state,
                snapshot,
                waiter.classifier.clone(),
                &waiter.context,
                waiter.deadline,
            )?;
            state.waiters.remove(token);
            Self::prune(&mut state);
            Ok((data, None, None))
        } else {
            drop(load);
            waiter.expires = (now_ms() + self.config.ttl).min(waiter.deadline);
            let mut state = self.state.lock().expect("snapshot state");
            if !state.waiters.contains_key(token) {
                return Err(SnapshotError::new(
                    Status::Cancelled,
                    "reservation released during preparation",
                ));
            }
            waiter.busy = false;
            state.waiters.insert(token.to_owned(), waiter.clone());
            drop(state);
            claim.keep = true;
            Ok(self.loading(token, &waiter, "preparation step exhausted"))
        }
    }

    fn before_source_read(&self) {
        #[cfg(test)]
        {
            let hook = self.read_hook.lock().expect("read hook").take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    fn matches_prefix(current: SourceStamp, pinned: SourceStamp) -> bool {
        current == pinned || current.identity == pinned.identity && current.size > pinned.size
    }

    fn step(
        &self,
        slot: &LoadSlot,
        load: &mut Load,
        read_bound: usize,
        events_bound: usize,
        lowering_events: usize,
        cancel: &Cancellation,
        deadline: u64,
        usage: &mut [u64; 18],
    ) -> Result<(), SnapshotError> {
        cancel.check(deadline)?;
        let read_start = usage[1];
        if !Self::matches_prefix(
            SourceStamp::of(&load.file.metadata().map_err(io_error)?),
            slot.stamp,
        ) {
            return Err(SnapshotError::new(
                Status::Changed,
                "open source changed during preparation",
            ));
        }
        if !load.origin_complete {
            let fence_size = (slot.stamp.size as usize).min(64);
            let count = fence_size
                .saturating_sub(load.origin_fence.len())
                .min(read_bound);
            if count == 0 && fence_size > load.origin_fence.len() {
                return Err(SnapshotError::new(
                    Status::SourceLimit,
                    "pinned fence exceeds read budget",
                ));
            }
            load.file
                .seek(SeekFrom::Start(
                    slot.stamp.size - fence_size as u64 + load.origin_fence.len() as u64,
                ))
                .map_err(io_error)?;
            let start = load.origin_fence.len();
            load.origin_fence.resize(start + count, 0);
            self.before_source_read();
            load.file
                .read_exact(&mut load.origin_fence[start..])
                .map_err(io_error)?;
            usage[1] += count as u64;
            if load.origin_fence.len() == fence_size {
                load.origin_complete = true;
                load.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
            }
            return Ok(());
        }
        if let Some(previous) = &load.previous {
            if !load.prefix_checked {
                let count = previous
                    .fence
                    .len()
                    .saturating_sub(load.prefix_fence.len())
                    .min(read_bound);
                if count == 0 && previous.fence.len() > load.prefix_fence.len() {
                    return Err(SnapshotError::new(
                        Status::SourceLimit,
                        "append fence exceeds read budget",
                    ));
                }
                load.file
                    .seek(SeekFrom::Start(
                        previous.committed_bytes - previous.fence.len() as u64
                            + load.prefix_fence.len() as u64,
                    ))
                    .map_err(io_error)?;
                let start = load.prefix_fence.len();
                load.prefix_fence.resize(start + count, 0);
                self.before_source_read();
                load.file
                    .read_exact(&mut load.prefix_fence[start..])
                    .map_err(io_error)?;
                usage[1] += count as u64;
                if load.prefix_fence.len() < previous.fence.len() {
                    return Ok(());
                }
                let fence = std::mem::take(&mut load.prefix_fence);
                if fence == previous.fence {
                    let keep = previous
                        .chunks
                        .len()
                        .saturating_sub(usize::from(previous.provisional_tail));
                    load.chunks = previous.chunks[..keep].to_vec();
                    load.count = load.chunks.iter().map(|chunk| chunk.entries.len()).sum();
                    load.committed = previous.committed_bytes;
                    load.offset = previous.committed_bytes;
                    load.pending_start = previous.committed_bytes;
                    load.provider = Some(Provider::Claude);
                    load.session_id = Some(previous.session_id.clone());
                    if !previous.provisional_tail && load.previous_index_compatible {
                        load.activity = previous.activity.as_ref().clone();
                        load.indexed = previous.event_count;
                    }
                    load.fence = fence;
                    load.prefix_checked = true;
                } else {
                    load.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
                    load.previous = None;
                    usage[4] += 1;
                }
                return Ok(());
            }
        }
        if load.indexed < load.count {
            let stop = (load.indexed + events_bound).min(load.count);
            let first = load
                .chunks
                .partition_point(|chunk| chunk.start <= load.indexed)
                .saturating_sub(1);
            let entries: Vec<&Entry> = load.chunks[first..]
                .iter()
                .flat_map(|chunk| {
                    chunk
                        .entries
                        .iter()
                        .skip(load.indexed.saturating_sub(chunk.start))
                })
                .take(stop - load.indexed)
                .collect();
            load.activity = std::mem::take(&mut load.activity).append_tail(&entries, None);
            load.indexed = stop;
            usage[6] += 1;
            return Ok(());
        }
        if !load.decoded {
            if load.offset < slot.stamp.size
                && (load.pending.is_empty()
                    || load.provider == Some(Provider::Codex)
                    || !load.pending.contains(&b'\n'))
            {
                let count = (slot.stamp.size - load.offset).min(read_bound as u64) as usize;
                let start = load.pending.len();
                load.pending.resize(start + count, 0);
                self.before_source_read();
                load.file
                    .read_exact(&mut load.pending[start..])
                    .map_err(io_error)?;
                load.offset += count as u64;
                usage[1] += count as u64;
                if !Self::matches_prefix(
                    SourceStamp::of(&load.file.metadata().map_err(io_error)?),
                    slot.stamp,
                ) {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "source changed while reading",
                    ));
                }
            }
            let eof = load.offset == slot.stamp.size;
            if load.provider.is_none() {
                let mut start = 0;
                for end in memchr::memchr_iter(b'\n', &load.pending) {
                    if !load.pending[start..end].iter().all(u8::is_ascii_whitespace) {
                        load.provider = Some(sniff_provider(&load.pending[start..end]));
                        break;
                    }
                    start = end + 1;
                }
                if eof && load.provider.is_none() {
                    load.provider = Some(sniff_provider(&load.pending));
                }
            }
            if load.provider == Some(Provider::Codex) {
                let mut line_start = 0;
                for end in memchr::memchr_iter(b'\n', &load.pending) {
                    if end - line_start > self.config.entry {
                        return Err(SnapshotError::new(
                            Status::EntryLimit,
                            "source entry exceeds owner bound",
                        ));
                    }
                    line_start = end + 1;
                }
                if load.pending.len() - line_start > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                if !eof {
                    return Ok(());
                }
                let source_events = memchr::memchr_iter(b'\n', &load.pending).count()
                    + usize::from(!load.pending.ends_with(b"\n"));
                if source_events > lowering_events {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "nonincremental provider lowering exceeds event work bound",
                    ));
                }
                usage[15] += 1;
                usage[16] += load.pending.len() as u64;
                usage[2] += load.pending.len() as u64;
                let parsed = parse_transcript_bytes(&load.pending).map_err(|error| {
                    SnapshotError::new(Status::ParseError, format!("{error:?}"))
                })?;
                usage[3] += parsed.entries.len() as u64;
                if parsed.entries.len() > lowering_events {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "lowered provider entries exceed work bound",
                    ));
                }
                load.session_id = parsed
                    .entries
                    .iter()
                    .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
                load.count = parsed.entries.len();
                load.chunks
                    .push(Arc::new(EntryChunk::new(0, parsed.entries)));
                load.committed = line_start as u64;
                load.provisional = line_start < load.pending.len();
                load.fence = load.pending[line_start.saturating_sub(64)..line_start].to_vec();
                load.pending.clear();
            } else {
                let mut entries = Vec::new();
                let mut consumed = 0;
                let mut lines = 0;
                for end in memchr::memchr_iter(b'\n', &load.pending) {
                    if lines >= events_bound {
                        break;
                    }
                    if end - consumed > self.config.entry {
                        return Err(SnapshotError::new(
                            Status::EntryLimit,
                            "source entry exceeds owner bound",
                        ));
                    }
                    crate::parse::parse_line(&load.pending[consumed..end], &mut entries, &|_| true)
                        .map_err(|error| {
                            SnapshotError::new(Status::ParseError, format!("{error:?}"))
                        })?;
                    let fresh = &load.pending[consumed..=end];
                    if fresh.len() >= 64 {
                        load.fence = fresh[fresh.len() - 64..].to_vec();
                    } else {
                        load.fence.extend_from_slice(fresh);
                        if load.fence.len() > 64 {
                            load.fence.drain(..load.fence.len() - 64);
                        }
                    }
                    usage[2] += (end + 1 - consumed) as u64;
                    consumed = end + 1;
                    lines += 1;
                }
                usage[3] += lines as u64;
                if !entries.is_empty() {
                    let count = entries.len();
                    if load.session_id.is_none() {
                        load.session_id = entries
                            .iter()
                            .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
                    }
                    load.chunks
                        .push(Arc::new(EntryChunk::new(load.count, entries)));
                    load.count += count;
                }
                if consumed > 0 {
                    load.pending.drain(..consumed);
                    load.pending_start += consumed as u64;
                    load.committed = load.pending_start;
                }
                if load.pending.contains(&b'\n') {
                    return Ok(());
                }
                if load.pending.len() > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                if !eof || lines >= events_bound && !load.pending.is_empty() {
                    return Ok(());
                }
                if !load.pending.is_empty() {
                    let mut tail = Vec::new();
                    crate::parse::parse_line(&load.pending, &mut tail, &|_| true).map_err(
                        |error| SnapshotError::new(Status::ParseError, format!("{error:?}")),
                    )?;
                    usage[2] += load.pending.len() as u64;
                    usage[3] += 1;
                    let count = tail.len();
                    if load.session_id.is_none() {
                        load.session_id = tail
                            .iter()
                            .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
                    }
                    load.chunks
                        .push(Arc::new(EntryChunk::new(load.count, tail)));
                    load.count += count;
                    load.provisional = true;
                    load.pending.clear();
                }
            }
            load.decoded = true;
        }
        if load.indexed < load.count {
            return Ok(());
        }
        if !load.sealed {
            let fence_size = load.origin_fence.len();
            let remaining = read_bound.saturating_sub((usage[1] - read_start) as usize);
            let count = fence_size
                .saturating_sub(load.seal_fence.len())
                .min(remaining);
            if count == 0 && fence_size > load.seal_fence.len() {
                if usage[1] > read_start {
                    return Ok(());
                }
                return Err(SnapshotError::new(
                    Status::SourceLimit,
                    "publication fence exceeds read budget",
                ));
            }
            load.file
                .seek(SeekFrom::Start(
                    slot.stamp.size - fence_size as u64 + load.seal_fence.len() as u64,
                ))
                .map_err(io_error)?;
            let start = load.seal_fence.len();
            load.seal_fence.resize(start + count, 0);
            self.before_source_read();
            load.file
                .read_exact(&mut load.seal_fence[start..])
                .map_err(io_error)?;
            usage[1] += count as u64;
            if load.seal_fence.len() < fence_size {
                return Ok(());
            }
            if load.seal_fence != load.origin_fence {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "pinned source prefix fence changed",
                ));
            }
            load.sealed = true;
        }
        if !Self::matches_prefix(
            SourceStamp::of(&load.file.metadata().map_err(io_error)?),
            slot.stamp,
        ) || !Self::matches_prefix(
            SourceStamp::of(&std::fs::metadata(&load.path).map_err(io_error)?),
            slot.stamp,
        ) {
            return Err(SnapshotError::new(
                Status::Changed,
                "source changed before publication",
            ));
        }
        let session_id = load.session_id.clone().unwrap_or_else(|| {
            load.path
                .file_stem()
                .expect("source filename")
                .to_string_lossy()
                .into_owned()
        });
        load.result = Some(Arc::new(TranscriptSnapshot {
            id: self.token("snapshot"),
            canonical_path: load.path.clone(),
            stamp: slot.stamp,
            provider: load.provider.unwrap_or(Provider::Claude),
            session_id,
            chunks: load.chunks.clone(),
            activity: Arc::new(load.activity.clone()),
            committed_bytes: load.committed,
            provisional_tail: load.provisional,
            fence: load.fence.clone(),
            event_count: load.count,
        }));
        load.pending = Vec::new();
        Ok(())
    }

    fn is_graph_request(request: &Value) -> bool {
        request.get("query").is_some_and(|query| {
            query.get("subagents").and_then(Value::as_bool) == Some(true)
                || matches!(
                    query.get("kind").and_then(Value::as_str),
                    Some("deep_predicate_inputs" | "sidechain_membership" | "direct_sidechains")
                )
        })
    }

    fn release_graph_state(state: &mut StoreState, graph: &GraphCursor) {
        if let Some(pending) = &graph.pending {
            state.waiters.remove(&pending.token);
        }
        for node in graph.nodes.iter().skip(1).filter(|node| !node.transferred) {
            if let Some(token) = node.description["handle"]["lease_id"].as_str() {
                if state
                    .leases
                    .get(token)
                    .is_some_and(|lease| lease.claimant == graph.claimant)
                {
                    state.leases.remove(token);
                }
            }
        }
    }

    fn graph(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut bounds = limits(request)?;
        cancel.check(bounds.deadline_unix_ms)?;
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        let root_handle = view
            .get("handle")
            .ok_or_else(|| invalid("missing root handle"))?
            .clone();
        let (root, description) =
            self.pin_scope_for_work(&root_handle, context, bounds.deadline_unix_ms)?;
        bounds.deadline_unix_ms = bounds
            .deadline_unix_ms
            .min(number(&description, "lease_expires_unix_ms")? as u64);
        if !classifier_eq(&view["classifier"], &description["classifier"])? {
            return Err(invalid("view classifier differs from root generation"));
        }
        let direct = str_field(&request["query"], "kind")? == "direct_sidechains";
        let selected = if direct {
            let ids = request["query"]
                .get("dispatch_ids")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("missing dispatch ids"))?;
            if ids.len() > 256
                || ids.iter().any(|id| {
                    id.as_str()
                        .is_none_or(|id| id.is_empty() || id.chars().count() > 256)
                })
            {
                return Err(invalid("direct sidechain dispatch ids exceed their bound"));
            }
            if str_field(&request["query"], "order")? != "forward" {
                return Err(invalid("direct sidechains require forward order"));
            }
            !ids.is_empty()
        } else {
            true
        };
        let mut tasks = Vec::new();
        let attachments = view
            .get("attachments")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing attachments"))?;
        for attachment in attachments.iter().rev().filter(|_| !direct) {
            tasks.push(GraphTask::Visit {
                path: PathBuf::from(
                    attachment
                        .as_str()
                        .ok_or_else(|| invalid("invalid attachment"))?,
                ),
                depth: 1,
                spawned_by: None,
            });
        }
        if selected {
            tasks.push(GraphTask::List {
                parent: root.canonical_path.clone(),
                depth: 1,
            });
        }
        let identity = root.stamp.identity;
        let graph = GraphCursor {
            claimant: str_field(context, "claimant")?.to_owned(),
            context: context.clone(),
            request: request.clone(),
            root_handle,
            remaining: bounds,
            nodes: vec![GraphNode {
                path: root.canonical_path.clone(),
                depth: 0,
                spawned_by: None,
                snapshot: root,
                description,
                transferred: false,
            }],
            seen: HashSet::from([identity]),
            tasks,
            listing: None,
            pending: None,
            prepared: false,
            root_checked: false,
            projection_at: 0,
            pending_record: None,
            published_members: Vec::new(),
            expires: (now_ms() + self.config.ttl).min(bounds.deadline_unix_ms),
            accounted: 0,
        };
        self.graph_step(&self.token("graph"), graph, cancel, usage)
    }

    fn graph_member_request(graph: &GraphCursor, index: usize) -> Value {
        let mut request = graph.request.clone();
        request["view"].insert("attachments", json!([]));
        request["view"].insert("handle", graph.nodes[index].description["handle"].clone());
        request["view"].insert(
            "classifier",
            graph.nodes[index].description["classifier"].clone(),
        );
        if index > 0 {
            request["view"].insert("selectors", json!([]));
        }
        if request["query"].get("subagents").is_some() {
            request["query"].insert("subagents", json!(false));
        }
        request
    }

    fn rollback_graph_page(state: &mut StoreState, graph: &mut GraphCursor) {
        for index in graph.published_members.drain(..) {
            graph.nodes[index].transferred = false;
        }
        Self::release_graph_state(state, graph);
    }

    fn graph_step(
        &self,
        token: &str,
        mut graph: GraphCursor,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let (data, complete) = match self.graph_work(&mut graph, cancel, usage) {
            Ok(GraphYield::Complete(data)) => (data, true),
            Ok(GraphYield::Pending(data)) => (data, false),
            Err(error) => {
                Self::rollback_graph_page(
                    &mut self.state.lock().expect("snapshot state"),
                    &mut graph,
                );
                return Err(error);
            }
        };
        let output = match encoded_size(&data, graph.remaining.max_output_bytes.min(MAX_DATA_BYTES))
        {
            Ok(bytes) => bytes,
            Err(error) => {
                Self::rollback_graph_page(
                    &mut self.state.lock().expect("snapshot state"),
                    &mut graph,
                );
                return Err(error);
            }
        };
        graph.remaining.max_output_bytes = graph.remaining.max_output_bytes.saturating_sub(output);
        graph.expires = (now_ms() + self.config.ttl).min(graph.remaining.deadline_unix_ms);
        if !complete {
            let charge = crate::snapshot_memory::value_charge(&graph.request);
            graph.accounted = size_of::<GraphCursor>()
                + charge.owned_capacity_bytes
                + charge.opaque_dom_accounted_bytes
                + graph.nodes.capacity() * size_of::<GraphNode>()
                + graph
                    .nodes
                    .iter()
                    .map(|node| {
                        let charge = crate::snapshot_memory::value_charge(&node.description);
                        node.path.as_os_str().len()
                            + charge.owned_capacity_bytes
                            + charge.opaque_dom_accounted_bytes
                    })
                    .sum::<usize>()
                + graph.tasks.capacity() * size_of::<GraphTask>()
                + graph
                    .pending_record
                    .as_ref()
                    .map_or(0, |(_, record)| record.capacity())
                + graph.listing.as_ref().map_or(0, |listing| {
                    listing
                        .children
                        .iter()
                        .map(|path| path.as_os_str().len())
                        .sum::<usize>()
                });
        }
        let mut state = self.state.lock().expect("snapshot state");
        if let Err(error) = self.lease(&state, &graph.root_handle, &graph.context) {
            Self::rollback_graph_page(&mut state, &mut graph);
            return Err(error);
        }
        if complete {
            graph.published_members.clear();
            Self::release_graph_state(&mut state, &graph);
            return Ok((data, None, None));
        }
        if state.graphs.len() >= self.lease_cap(&graph.context)? {
            Self::rollback_graph_page(&mut state, &mut graph);
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "graph cursor admission exhausted",
            ));
        }
        if let Err(error) = self.admit_memory(&mut state, &graph.context, graph.accounted) {
            Self::rollback_graph_page(&mut state, &mut graph);
            return Err(error);
        }
        graph.published_members.clear();
        state.graphs.insert(token.to_owned(), graph);
        Ok((
            data,
            Some(token.to_owned()),
            Some("graph work incomplete".to_owned()),
        ))
    }

    fn graph_add_source(
        &self,
        graph: &mut GraphCursor,
        path: PathBuf,
        depth: usize,
        spawned_by: Option<String>,
        data: &Value,
    ) -> Result<(), SnapshotError> {
        let acquired = data
            .get("description")
            .ok_or_else(|| invalid("acquire returned no source description"))?;
        let (snapshot, description) = self.pin_scope_for_work(
            &acquired["handle"],
            &graph.context,
            graph.remaining.deadline_unix_ms,
        )?;
        let identity = snapshot.stamp.identity;
        if !graph.seen.insert(identity)
            && graph.request["query"]["kind"].as_str() != Some("direct_sidechains")
        {
            self.state
                .lock()
                .expect("snapshot state")
                .leases
                .remove(str_field(&description["handle"], "lease_id")?);
            return Ok(());
        }
        graph.nodes.push(GraphNode {
            path: path.clone(),
            depth,
            spawned_by,
            snapshot,
            description,
            transferred: false,
        });
        if graph.request["query"]["kind"].as_str() != Some("direct_sidechains") {
            graph.tasks.push(GraphTask::List {
                parent: path,
                depth: depth + 1,
            });
        }
        Ok(())
    }

    fn renew_graph_members(&self, graph: &mut GraphCursor) -> Result<(), SnapshotError> {
        let mut state = self.state.lock().expect("snapshot state");
        self.lease(&state, &graph.root_handle, &graph.context)?;
        for node in graph
            .nodes
            .iter_mut()
            .skip(1)
            .filter(|node| !node.transferred)
        {
            let handle = &node.description["handle"];
            let lease = self.lease(&state, handle, &graph.context)?;
            let expires = lease.expires.max(
                graph
                    .remaining
                    .deadline_unix_ms
                    .min(lease.absolute_deadline),
            );
            state
                .leases
                .get_mut(str_field(handle, "lease_id")?)
                .expect("validated lease")
                .expires = expires;
            node.description
                .insert("lease_expires_unix_ms", json!(expires));
        }
        Ok(())
    }

    fn graph_work(
        &self,
        graph: &mut GraphCursor,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<GraphYield, SnapshotError> {
        cancel.check(graph.remaining.deadline_unix_ms)?;
        self.pin_scope_for_work(
            &graph.root_handle,
            &graph.context,
            graph.remaining.deadline_unix_ms,
        )?;
        self.renew_graph_members(graph)?;
        let kind = str_field(&graph.request["query"], "kind")?.to_owned();
        let direct = kind == "direct_sidechains";
        let membership = direct || kind == "sidechain_membership";
        let inputs = kind == "deep_predicate_inputs";
        let boolean = !membership && !inputs;
        if boolean && !graph.root_checked {
            let request = Self::graph_member_request(graph, 0);
            let mut bounds = graph.remaining;
            bounds.max_output_bytes = bounds.max_output_bytes.min(MAX_DATA_BYTES);
            let projection = crate::snapshot_projection::project(
                &graph.nodes[0].snapshot,
                &request,
                &bounds,
                cancel,
                0,
            )?;
            graph.remaining.max_read_bytes = graph
                .remaining
                .max_read_bytes
                .saturating_sub(projection.read_bytes);
            graph.remaining.max_events =
                graph.remaining.max_events.saturating_sub(projection.events);
            if !projection.complete {
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "root predicate work incomplete",
                ));
            }
            graph.root_checked = true;
            if projection.data["value"].as_bool() == Some(true) {
                return Ok(GraphYield::Complete(projection.data));
            }
        }
        if let Some(pending) = graph.pending.take() {
            self.authority(
                &graph.context,
                Some(&std::fs::canonicalize(&pending.path).map_err(io_error)?),
            )?;
            let waiter = {
                let mut state = self.state.lock().expect("snapshot state");
                let waiter = state.waiters.get_mut(&pending.token).ok_or_else(|| {
                    SnapshotError::new(Status::StaleCursor, "graph source reservation expired")
                })?;
                waiter.context = graph.context.clone();
                waiter.clone()
            };
            let before_bytes = usage[1];
            let before_events = usage[3];
            let outcome = self.advance(&pending.token, waiter, cancel, usage)?;
            graph.remaining.max_read_bytes = graph
                .remaining
                .max_read_bytes
                .saturating_sub((usage[1] - before_bytes) as usize);
            graph.remaining.max_events = graph
                .remaining
                .max_events
                .saturating_sub((usage[3] - before_events) as usize);
            if outcome.1.is_some() {
                graph.pending = Some(pending);
            } else {
                self.graph_add_source(
                    graph,
                    pending.path,
                    pending.depth,
                    pending.spawned_by,
                    &outcome.0,
                )?;
            }
            return Ok(GraphYield::Pending(Value::new_null()));
        }
        let mut examined = 0usize;
        while !graph.prepared && examined < self.config.event_step {
            cancel.check(graph.remaining.deadline_unix_ms)?;
            if let Some(mut listing) = graph.listing.take() {
                loop {
                    if examined >= self.config.event_step {
                        graph.listing = Some(listing);
                        return Ok(GraphYield::Pending(Value::new_null()));
                    }
                    let Some(entry) = listing.entries.next() else {
                        break;
                    };
                    if graph.remaining.max_discovery_entries == 0 {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "graph discovery budget exhausted",
                        ));
                    }
                    graph.remaining.max_discovery_entries -= 1;
                    examined += 1;
                    usage[17] += 1;
                    let entry = entry.map_err(io_error)?;
                    let path = entry.path();
                    let name = entry.file_name();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                        && !name.to_string_lossy().starts_with("._")
                    {
                        if direct {
                            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                                continue;
                            };
                            let id = stem.strip_prefix("agent-").unwrap_or(stem);
                            if !graph.request["query"]["dispatch_ids"]
                                .as_array()
                                .expect("validated dispatch ids")
                                .iter()
                                .any(|selected| selected.as_str() == Some(id))
                            {
                                continue;
                            }
                        }
                        if listing.children.len() + graph.nodes.len() > graph.remaining.max_sources
                        {
                            return Err(SnapshotError::new(
                                Status::Incomplete,
                                "graph source discovery budget exhausted",
                            ));
                        }
                        listing.children.push(path);
                    }
                }
                listing.children.sort();
                for path in listing.children.into_iter().rev() {
                    let spawned_by = path
                        .file_stem()
                        .expect("sidechain filename")
                        .to_string_lossy()
                        .strip_prefix("agent-")
                        .unwrap_or_else(|| {
                            path.file_stem()
                                .expect("sidechain filename")
                                .to_str()
                                .unwrap_or("")
                        })
                        .to_owned();
                    graph.tasks.push(GraphTask::Visit {
                        path,
                        depth: listing.depth,
                        spawned_by: Some(spawned_by),
                    });
                }
                continue;
            }
            let Some(task) = graph.tasks.pop() else {
                graph.prepared = true;
                break;
            };
            match task {
                GraphTask::List { parent, depth } => {
                    let directory = parent
                        .parent()
                        .ok_or_else(|| invalid("source has no parent"))?
                        .join(
                            parent
                                .file_stem()
                                .ok_or_else(|| invalid("source has no stem"))?,
                        )
                        .join("subagents");
                    match std::fs::canonicalize(&directory) {
                        Ok(canonical) => {
                            self.authority(&graph.context, Some(&canonical))?;
                            graph.listing = Some(GraphListing {
                                entries: std::fs::read_dir(&directory).map_err(io_error)?,
                                children: Vec::new(),
                                depth,
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io_error(error)),
                    }
                }
                GraphTask::Visit {
                    path,
                    depth,
                    spawned_by,
                } => {
                    let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
                    self.authority(&graph.context, Some(&canonical))?;
                    let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                    if !metadata.is_file() {
                        return Err(invalid("graph source must be a file"));
                    }
                    if !direct && graph.seen.contains(&SourceStamp::of(&metadata).identity) {
                        continue;
                    }
                    if graph.nodes.len() >= graph.remaining.max_sources {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "graph source budget exhausted",
                        ));
                    }
                    let bounds = graph.remaining;
                    let acquire = json!({"schema":SCHEMA,"id":"graph-source","operation":"acquire","path":canonical.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":bounds.deadline_unix_ms,
                        "limits":{"max_read_bytes":bounds.max_read_bytes,"max_events":bounds.max_events,"max_items":bounds.max_items,"max_output_bytes":bounds.max_output_bytes,"max_discovery_entries":bounds.max_discovery_entries,"max_sources":bounds.max_sources}});
                    let before_bytes = usage[1];
                    let before_events = usage[3];
                    let outcome = self.acquire(&acquire, &graph.context, cancel, usage)?;
                    graph.remaining.max_read_bytes = graph
                        .remaining
                        .max_read_bytes
                        .saturating_sub((usage[1] - before_bytes) as usize);
                    graph.remaining.max_events = graph
                        .remaining
                        .max_events
                        .saturating_sub((usage[3] - before_events) as usize);
                    if let Some(token) = outcome.1 {
                        graph.pending = Some(GraphPending {
                            token,
                            path,
                            depth,
                            spawned_by,
                        });
                    } else {
                        self.graph_add_source(graph, path, depth, spawned_by, &outcome.0)?;
                    }
                    return Ok(GraphYield::Pending(Value::new_null()));
                }
            }
        }
        if !graph.prepared {
            return Ok(GraphYield::Pending(Value::new_null()));
        }
        let total = if membership || boolean {
            graph.nodes.len() - 1
        } else {
            graph.nodes.len()
        };
        let reverse =
            graph.request["query"].get("order").and_then(Value::as_str) == Some("reverse");
        let mut records = Vec::new();
        let mut output_bytes = 128usize;
        let page_limit = self.config.page_items.min(graph.remaining.max_items);
        while graph.projection_at < total || graph.pending_record.is_some() {
            cancel.check(graph.remaining.deadline_unix_ms)?;
            if records.len() >= page_limit {
                break;
            }
            let (index, record) = if let Some(pending) = graph.pending_record.take() {
                pending
            } else {
                let position = if reverse {
                    total - 1 - graph.projection_at
                } else {
                    graph.projection_at
                };
                let index = position + usize::from(membership || boolean);
                let node = &graph.nodes[index];
                self.validate_scope(&node.description["handle"], &graph.context)?;
                self.authority(&graph.context, Some(&node.snapshot.canonical_path))?;
                if membership {
                    if node.snapshot.session_id.len() > 256 {
                        return Err(SnapshotError::new(
                            Status::OutputLimit,
                            "graph session identity exceeds record bound",
                        ));
                    }
                    let record = json!({"path":node.path.to_string_lossy().as_ref(),"session_id":node.snapshot.session_id,"provider":node.snapshot.provider.as_str(),"depth":node.depth,"spawned_by":node.spawned_by,"description":node.description});
                    encoded_size(&record, MAX_DATA_BYTES)?;
                    graph.projection_at += 1;
                    (
                        index,
                        sonic_rs::to_string(&record).map_err(|error| invalid(error.to_string()))?,
                    )
                } else {
                    let request = Self::graph_member_request(graph, index);
                    let mut bounds = graph.remaining;
                    bounds.max_items = 1;
                    bounds.max_output_bytes = bounds.max_output_bytes.min(MAX_DATA_BYTES);
                    let projection = crate::snapshot_projection::project(
                        &node.snapshot,
                        &request,
                        &bounds,
                        cancel,
                        0,
                    )?;
                    graph.remaining.max_read_bytes = graph
                        .remaining
                        .max_read_bytes
                        .saturating_sub(projection.read_bytes);
                    graph.remaining.max_events =
                        graph.remaining.max_events.saturating_sub(projection.events);
                    if !projection.complete {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "graph member projection incomplete",
                        ));
                    }
                    graph.projection_at += 1;
                    if boolean {
                        if projection.data["value"].as_bool() == Some(true) {
                            return Ok(GraphYield::Complete(projection.data));
                        }
                        if graph.projection_at >= total {
                            break;
                        }
                        continue;
                    }
                    (
                        index,
                        projection.data["records_json"][0]
                            .as_str()
                            .ok_or_else(|| invalid("predicate member returned no record"))?
                            .to_owned(),
                    )
                }
            };
            let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)?;
            if output_bytes.saturating_add(record_bytes)
                > graph.remaining.max_output_bytes.min(MAX_DATA_BYTES)
            {
                graph.pending_record = Some((index, record));
                if records.is_empty() {
                    return Err(SnapshotError::new(
                        Status::OutputLimit,
                        "graph record exceeds remaining output budget",
                    ));
                }
                break;
            }
            output_bytes += record_bytes + 1;
            if membership {
                graph.nodes[index].transferred = true;
                graph.published_members.push(index);
            }
            records.push(record);
        }
        if boolean {
            return Ok(GraphYield::Complete(json!({"kind":"scalar","value":false})));
        }
        let data = json!({"kind":"records","record_schema":if membership {"cc-transcript.sidechain/1"} else {"cc-transcript.predicate-inputs/1"},"records_json":records});
        graph.remaining.max_items = graph
            .remaining
            .max_items
            .saturating_sub(data["records_json"].as_array().expect("records").len());
        if graph.projection_at >= total && graph.pending_record.is_none() {
            Ok(GraphYield::Complete(data))
        } else if graph.remaining.max_items == 0 {
            Err(SnapshotError::new(
                Status::Incomplete,
                "graph item budget exhausted",
            ))
        } else {
            Ok(GraphYield::Pending(data))
        }
    }

    fn project_request(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        mut bound: WorkLimits,
        next: usize,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(bound.deadline_unix_ms)?;
        bound.max_output_bytes = bound.max_output_bytes.min(self.config.output);
        if str_field(request, "operation")? == "hydrate" {
            return self.hydrate(request, context, cancel, bound, next);
        }
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        let handle = view
            .get("handle")
            .ok_or_else(|| invalid("missing view handle"))?;
        let (snapshot, description) =
            self.pin_scope_for_work(handle, context, bound.deadline_unix_ms)?;
        bound.deadline_unix_ms = bound
            .deadline_unix_ms
            .min(number(&description, "lease_expires_unix_ms")? as u64);
        if !classifier_eq(&view["classifier"], &description["classifier"])? {
            return Err(invalid("view classifier differs from leased generation"));
        }
        let mut page = bound;
        page.max_output_bytes = page.max_output_bytes.min(MAX_DATA_BYTES);
        page.max_items = page.max_items.min(self.config.page_items);
        let projection = if str_field(request, "operation")? == "mine" {
            let policy = request
                .get("policy")
                .ok_or_else(|| invalid("missing policy"))?;
            let key = sonic_rs::to_string(&json!([
                str_field(policy, "id")?,
                str_field(policy, "version")?
            ]))
            .expect("policy key");
            let callback = self
                .policies
                .lock()
                .expect("policies")
                .get(&key)
                .cloned()
                .ok_or_else(|| invalid("policy version is not registered with this owner"))?;
            callback(snapshot, request, &page, cancel, next)?
        } else {
            let mut local = request.clone();
            local["view"].insert("attachments", json!([]));
            crate::snapshot_projection::project(&snapshot, &local, &page, cancel, next)?
        };
        self.projection_result(request, context, bound, projection)
    }

    fn projection_result(
        &self,
        request: &Value,
        context: &Value,
        mut bound: WorkLimits,
        projection: Projection,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bytes = sonic_rs::to_vec(&projection.data)
            .map_err(|error| invalid(error.to_string()))?
            .len();
        if bytes > bound.max_output_bytes {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "projection exceeds request output budget",
            ));
        }
        let cursor = if let Some(next) = projection.next {
            bound.max_output_bytes = bound.max_output_bytes.saturating_sub(bytes);
            bound.max_read_bytes = bound.max_read_bytes.saturating_sub(projection.read_bytes);
            bound.max_events = bound.max_events.saturating_sub(projection.events);
            bound.max_items = bound.max_items.saturating_sub(projection.items);
            if bound.max_output_bytes == 0
                || bound.max_events == 0
                || bound.max_read_bytes == 0
                || bound.max_items == 0
            {
                None
            } else {
                let mut state = self.state.lock().expect("snapshot state");
                Self::prune(&mut state);
                if state.projections.len() >= self.lease_cap(context)? {
                    return Err(SnapshotError::new(
                        Status::LeaseLimit,
                        "projection cursor admission exhausted",
                    ));
                }
                let charge = crate::snapshot_memory::value_charge(request);
                self.admit_memory(
                    &mut state,
                    context,
                    charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
                )?;
                let token = self.token("projection");
                state.projections.insert(
                    token.clone(),
                    ProjectionCursor {
                        claimant: str_field(context, "claimant")?.to_owned(),
                        registry_generation: str_field(context, "registry_generation")?.to_owned(),
                        admission: str_field(context, "admission")?.to_owned(),
                        request: request.clone(),
                        limits: bound,
                        next,
                        expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
                    },
                );
                Some(token)
            }
        } else {
            None
        };
        Ok((
            projection.data,
            cursor,
            if projection.complete {
                None
            } else {
                Some(
                    projection
                        .reason
                        .unwrap_or_else(|| "projection work incomplete".to_owned()),
                )
            },
        ))
    }

    fn hydrate(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        mut bound: WorkLimits,
        next: usize,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let handles = request
            .get("handles")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing handles"))?;
        let windows = request
            .get("windows_json")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing windows_json"))?;
        let mut sessions = HashMap::new();
        for handle in handles.iter() {
            let (snapshot, description) = self.pin_scope_for_work(
                handle
                    .get("handle")
                    .ok_or_else(|| invalid("missing handle"))?,
                context,
                bound.deadline_unix_ms,
            )?;
            bound.deadline_unix_ms = bound
                .deadline_unix_ms
                .min(number(&description, "lease_expires_unix_ms")? as u64);
            let session = str_field(handle, "session_id")?;
            if session != snapshot.session_id {
                return Err(invalid("hydration session differs from handle"));
            }
            if sessions.insert(session, snapshot).is_some() {
                return Err(invalid("duplicate hydration session"));
            }
        }
        if next > windows.len() {
            return Err(invalid("hydration cursor out of range"));
        }
        let mut remaining = bound;
        remaining.max_output_bytes = remaining.max_output_bytes.min(MAX_DATA_BYTES);
        let mut output = Vec::new();
        let mut at = next;
        let mut read_bytes = 0;
        let mut events = 0;
        while at < windows.len() && output.len() < bound.max_items.min(self.config.page_items) {
            cancel.check(bound.deadline_unix_ms)?;
            let text = windows[at]
                .as_str()
                .ok_or_else(|| invalid("window must be JSON text"))?;
            if text.len() > remaining.max_read_bytes {
                break;
            }
            let window = crate::context::ContextWindow::from_json(text)
                .map_err(|error| invalid(error.to_string()))?;
            let item = if let Some(snapshot) = sessions.get(window.anchor.session_id.as_str()) {
                let mut single = request.clone();
                single.insert("windows_json", json!([text]));
                let projected =
                    crate::snapshot_projection::project(snapshot, &single, &remaining, cancel, 0)?;
                if !projected.complete {
                    break;
                }
                read_bytes += projected.read_bytes;
                events += projected.events;
                remaining.max_read_bytes = remaining
                    .max_read_bytes
                    .saturating_sub(projected.read_bytes);
                remaining.max_events = remaining.max_events.saturating_sub(projected.events);
                let mut item = projected.data["windows"][0].clone();
                item.insert("input_index", json!(at));
                item
            } else {
                read_bytes += text.len();
                remaining.max_read_bytes -= text.len();
                json!({"input_index": at, "availability": "missing_ref", "rendered": null})
            };
            let bytes = sonic_rs::to_vec(&item)
                .map_err(|error| invalid(error.to_string()))?
                .len();
            if bytes > remaining.max_output_bytes.saturating_sub(64) {
                break;
            }
            remaining.max_output_bytes -= bytes;
            output.push(item);
            at += 1;
        }
        let count = output.len();
        self.projection_result(
            request,
            context,
            bound,
            Projection {
                data: json!({"kind": "hydrated", "windows": output}),
                complete: at == windows.len(),
                next: (at < windows.len() && at > next).then_some(at),
                reason: (at < windows.len()).then(|| "hydration work incomplete".to_owned()),
                read_bytes,
                events,
                items: count,
            },
        )
    }

    fn discover(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bound = limits(request)?;
        cancel.check(bound.deadline_unix_ms)?;
        if str_field(request, "operation")? == "resolve" {
            return self.resolve(request, context, cancel, usage);
        }
        let roots = request
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing roots"))?;
        let mut paths = Vec::new();
        for root in roots.iter() {
            let path = std::fs::canonicalize(root.as_str().ok_or_else(|| invalid("invalid root"))?)
                .map_err(io_error)?;
            self.authority(context, Some(&path))?;
            paths.push(path);
        }
        let previous = if let Some(token) = request.get("checkpoint").and_then(Value::as_str) {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            let checkpoint = state.checkpoints.get(token).ok_or_else(|| {
                SnapshotError::new(Status::StaleCursor, "discovery checkpoint expired")
            })?;
            if checkpoint.claimant != str_field(context, "claimant")?
                || sonic_rs::to_vec(&checkpoint.roots).ok()
                    != sonic_rs::to_vec(&request["roots"]).ok()
            {
                return Err(SnapshotError::new(
                    Status::StaleCursor,
                    "checkpoint scope differs",
                ));
            }
            checkpoint.inventory.clone()
        } else {
            HashMap::new()
        };
        let token = self.token("discovery");
        let cursor = DiscoveryCursor {
            claimant: str_field(context, "claimant")?.to_owned(),
            request: request.clone(),
            context: context.clone(),
            limits: bound,
            roots: paths,
            directories: Vec::new(),
            seen: HashSet::new(),
            seen_directories: HashSet::new(),
            examined: 0,
            sources: 0,
            emitted: 0,
            output_bytes: 0,
            inventory: HashMap::new(),
            previous,
            removed: Vec::new(),
            walking: true,
            expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
            accounted: 0,
        };
        self.scan(&token, cursor, cancel, usage)
    }

    fn scan(
        &self,
        token: &str,
        mut scan: DiscoveryCursor,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(scan.limits.deadline_unix_ms)?;
        let mut output = Vec::new();
        let page_items = self
            .config
            .page_items
            .min(MAX_DATA_BYTES / (4096 * 6 + 1024));
        let stop = scan
            .examined
            .saturating_add(self.config.event_step)
            .min(scan.limits.max_discovery_entries);
        while scan.walking
            && scan.examined < stop
            && output.len() < page_items
            && scan.emitted < scan.limits.max_items
        {
            cancel.check(scan.limits.deadline_unix_ms)?;
            let path = if scan.directories.is_empty() {
                let Some(path) = scan.roots.pop() else {
                    scan.walking = false;
                    scan.removed = scan
                        .previous
                        .drain()
                        .filter(|(path, _)| !scan.inventory.contains_key(path))
                        .map(|(_, mut value)| {
                            value.insert("state", json!("removed"));
                            value
                        })
                        .collect();
                    break;
                };
                self.authority(&scan.context, Some(&path))?;
                let metadata = std::fs::metadata(&path).map_err(io_error)?;
                if metadata.is_dir() {
                    if !scan
                        .seen_directories
                        .insert(SourceStamp::of(&metadata).identity)
                    {
                        continue;
                    }
                    scan.directories
                        .push(std::fs::read_dir(path).map_err(io_error)?);
                    continue;
                }
                if !metadata.is_file() {
                    return Err(invalid("discovery roots must be files or directories"));
                }
                scan.examined += 1;
                usage[17] += 1;
                path
            } else {
                let Some(entry) = scan.directories.last_mut().expect("directory").next() else {
                    scan.directories.pop();
                    continue;
                };
                let entry = entry.map_err(io_error)?;
                scan.examined += 1;
                usage[17] += 1;
                let path = entry.path();
                let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
                self.authority(&scan.context, Some(&canonical))?;
                let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                if metadata.is_dir() {
                    if scan
                        .request
                        .get("follow_directory_symlinks")
                        .and_then(Value::as_bool)
                        == Some(false)
                        && entry.file_type().map_err(io_error)?.is_symlink()
                    {
                        continue;
                    }
                    scan.roots.push(canonical);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                path
            };
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            self.authority(&scan.context, Some(&path))?;
            if scan.sources >= scan.limits.max_sources {
                return Ok((
                    json!({"kind":"discovered","entries":output,"checkpoint":null}),
                    None,
                    Some("source discovery budget exhausted".to_owned()),
                ));
            }
            let metadata = std::fs::metadata(&path).map_err(io_error)?;
            let stamp = SourceStamp::of(&metadata);
            if !scan.seen.insert(stamp.identity)
                && scan
                    .request
                    .get("preserve_aliases")
                    .and_then(Value::as_bool)
                    != Some(true)
            {
                continue;
            }
            scan.sources += 1;
            let path = path.to_string_lossy().into_owned();
            let revision = stamp.revision();
            let value = json!({"path":path,"revision":revision,"state":"present","size":stamp.size,"mtime_ns":stamp.mtime_ns.to_string()});
            let changed = scan
                .previous
                .get(&path)
                .and_then(|old| old.get("revision"))
                .and_then(Value::as_str)
                != Some(revision.as_str());
            let bytes = sonic_rs::to_vec(&value)
                .map_err(|error| invalid(error.to_string()))?
                .len();
            scan.accounted += path.capacity() + bytes * 2 + size_of::<Value>();
            scan.inventory.insert(path, value.clone());
            if changed {
                if scan.output_bytes.saturating_add(bytes)
                    > scan.limits.max_output_bytes.saturating_sub(128)
                {
                    return Ok((
                        json!({"kind":"discovered","entries":output,"checkpoint":null}),
                        None,
                        Some("discovery output budget exhausted".to_owned()),
                    ));
                }
                scan.output_bytes += bytes;
                scan.emitted += 1;
                output.push(value);
            }
        }
        while !scan.walking
            && !scan.removed.is_empty()
            && output.len() < page_items
            && scan.emitted < scan.limits.max_items
        {
            let value = scan.removed.pop().expect("removed source");
            let bytes = sonic_rs::to_vec(&value)
                .map_err(|error| invalid(error.to_string()))?
                .len();
            if scan.output_bytes.saturating_add(bytes)
                > scan.limits.max_output_bytes.saturating_sub(128)
            {
                return Ok((
                    json!({"kind":"discovered","entries":output,"checkpoint":null}),
                    None,
                    Some("discovery output budget exhausted".to_owned()),
                ));
            }
            scan.output_bytes += bytes;
            scan.emitted += 1;
            output.push(value);
        }
        let complete = !scan.walking && scan.removed.is_empty();
        let exhausted = scan.examined >= scan.limits.max_discovery_entries
            || scan.emitted >= scan.limits.max_items;
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        if state.discoveries.len() + state.checkpoints.len() >= self.lease_cap(&scan.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "discovery cursor admission exhausted",
            ));
        }
        self.admit_memory(&mut state, &scan.context, scan.accounted)?;
        if complete {
            let checkpoint = self.token("checkpoint");
            state.checkpoints.insert(
                checkpoint.clone(),
                Checkpoint {
                    claimant: scan.claimant,
                    roots: scan.request["roots"].clone(),
                    inventory: scan.inventory,
                    expires: now_ms() + self.config.ttl,
                    accounted: scan.accounted,
                },
            );
            Ok((
                json!({"kind":"discovered","entries":output,"checkpoint":checkpoint}),
                None,
                None,
            ))
        } else if exhausted {
            Ok((
                json!({"kind":"discovered","entries":output,"checkpoint":null}),
                None,
                Some("cumulative discovery budget exhausted".to_owned()),
            ))
        } else {
            scan.expires = (now_ms() + self.config.ttl).min(scan.limits.deadline_unix_ms);
            state.discoveries.insert(token.to_owned(), scan);
            Ok((
                json!({"kind":"discovered","entries":output,"checkpoint":null}),
                Some(token.to_owned()),
                Some("discovery page incomplete".to_owned()),
            ))
        }
    }

    fn requested_location<'a>(stem: &str, wanted: &'a HashSet<String>) -> Option<&'a str> {
        if let Some(id) = wanted.get(stem) {
            return Some(id);
        }
        stem.match_indices('-')
            .find_map(|(index, _)| wanted.get(&stem[index + 1..]).map(String::as_str))
    }

    fn location_candidate(
        &self,
        path: &Path,
        scope: &[PathBuf],
        context: &Value,
    ) -> Result<Option<SourceStamp>, SnapshotError> {
        if !scope.iter().any(|root| path.starts_with(root)) {
            return Ok(None);
        }
        let canonical = match std::fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        self.authority(context, Some(&canonical))?;
        let metadata = match std::fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        Ok(metadata.is_file().then(|| SourceStamp::of(&metadata)))
    }

    fn remember_locations(&self, entries: &[(String, PathBuf)], context: &Value) {
        if entries.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        let added = entries
            .iter()
            .map(|(id, path)| id.len() + path.as_os_str().len() + size_of::<LocatedPath>())
            .sum::<usize>();
        while state.locations.len() + entries.len() > 4096 {
            let Some(oldest) = state
                .locations
                .iter()
                .min_by_key(|(_, location)| location.expires)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            state.locations.remove(&oldest);
        }
        if self.admit_memory(&mut state, context, added).is_err() {
            return;
        }
        let expires = now_ms().saturating_add(self.config.ttl.saturating_mul(10));
        for (id, path) in entries {
            if id.len() + path.as_os_str().len() <= 8192 {
                state.locations.insert(
                    id.clone(),
                    LocatedPath {
                        path: path.clone(),
                        expires,
                    },
                );
            }
        }
    }

    fn locate(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bound = limits(request)?;
        cancel.check(bound.deadline_unix_ms)?;
        let requested = request
            .get("session_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing session_ids"))?;
        if requested.is_empty() || requested.len() > 1024 || requested.len() > bound.max_items {
            return Err(invalid("location request exceeds session item bound"));
        }
        let mut ids = Vec::with_capacity(requested.len());
        let mut wanted = HashSet::with_capacity(requested.len());
        for value in requested.iter() {
            let id = value
                .as_str()
                .ok_or_else(|| invalid("invalid session id"))?;
            if id.is_empty() || id.len() > 256 || !wanted.insert(id.to_owned()) {
                return Err(invalid("invalid or duplicate session id"));
            }
            ids.push(id.to_owned());
        }
        let roots = request
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing roots"))?;
        if roots.is_empty() || roots.len() > 64 {
            return Err(invalid("invalid root count"));
        }
        let mut scope = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            let path = PathBuf::from(root.as_str().ok_or_else(|| invalid("invalid root"))?);
            let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
            self.authority(context, Some(&canonical))?;
            scope.push(path);
        }
        let cached = {
            let state = self.state.lock().expect("snapshot state");
            ids.iter()
                .filter_map(|id| {
                    state
                        .locations
                        .get(id)
                        .map(|location| (id.clone(), location.path.clone()))
                })
                .collect::<Vec<_>>()
        };
        let mut found = HashSet::new();
        let mut pending = VecDeque::new();
        let mut refreshed = Vec::new();
        for (id, path) in cached {
            if let Some(stamp) = self.location_candidate(&path, &scope, context)? {
                found.insert(id.clone());
                pending.push_back(json!({"session_id":id,"status":"ok","path":path.to_string_lossy().as_ref(),"revision":stamp.revision()}));
                refreshed.push((id, path));
            }
        }
        self.remember_locations(&refreshed, context);
        let token = self.token("location");
        let cursor = LocateCursor {
            claimant: str_field(context, "claimant")?.to_owned(),
            context: context.clone(),
            limits: bound,
            ids,
            wanted,
            found,
            roots: scope.clone(),
            scope,
            directories: Vec::new(),
            seen_directories: HashSet::new(),
            pending,
            examined: 0,
            emitted: 0,
            output_bytes: 0,
            finished: false,
            exhausted: false,
            expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
        };
        self.locate_step(&token, cursor, cancel, usage)
    }

    fn locate_step(
        &self,
        token: &str,
        mut cursor: LocateCursor,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(cursor.limits.deadline_unix_ms)?;
        let mut output = Vec::new();
        let mut page_bytes = 64usize;
        let stop = cursor
            .examined
            .saturating_add(self.config.event_step)
            .min(cursor.limits.max_discovery_entries);
        let mut updates = Vec::new();
        loop {
            while let Some(mut item) = cursor.pending.pop_front() {
                if item["status"].as_str() == Some("ok") {
                    let path = PathBuf::from(str_field(&item, "path")?);
                    let id = str_field(&item, "session_id")?.to_owned();
                    if let Some(stamp) =
                        self.location_candidate(&path, &cursor.scope, &cursor.context)?
                    {
                        item.insert("revision", json!(stamp.revision()));
                    } else {
                        cursor.found.remove(&id);
                        continue;
                    }
                }
                let bytes = encoded_size(&item, MAX_DATA_BYTES)?;
                if cursor.output_bytes + bytes + 2
                    > cursor.limits.max_output_bytes.saturating_sub(64)
                {
                    return Err(SnapshotError::new(
                        Status::OutputLimit,
                        "location output budget exhausted",
                    ));
                }
                if page_bytes + bytes + 2 > MAX_DATA_BYTES || output.len() >= 1024 {
                    cursor.pending.push_front(item);
                    break;
                }
                cursor.output_bytes += bytes + 2;
                cursor.emitted += 1;
                page_bytes += bytes + 2;
                output.push(item);
            }
            if !cursor.pending.is_empty() || cursor.finished || page_bytes >= MAX_DATA_BYTES {
                break;
            }
            if cursor.found.len() == cursor.ids.len() {
                cursor.finished = true;
                break;
            }
            if cursor.examined >= cursor.limits.max_discovery_entries
                || cursor.found.len() >= cursor.limits.max_sources
            {
                cursor.exhausted = true;
                cursor.finished = true;
            } else if cursor.examined >= stop {
                break;
            } else {
                let path = if let Some(reader) = cursor.directories.last_mut() {
                    match reader.next() {
                        Some(Ok(entry)) => {
                            cursor.examined += 1;
                            usage[17] += 1;
                            entry.path()
                        }
                        Some(Err(error)) => return Err(io_error(error)),
                        None => {
                            cursor.directories.pop();
                            continue;
                        }
                    }
                } else if let Some(root) = cursor.roots.pop() {
                    root
                } else {
                    cursor.finished = true;
                    for id in &cursor.ids {
                        if !cursor.found.contains(id) {
                            cursor.pending.push_back(json!({"session_id":id,"status":"missing","path":null,"revision":null}));
                        }
                    }
                    continue;
                };
                let canonical = match std::fs::canonicalize(&path) {
                    Ok(path) => path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error(error)),
                };
                self.authority(&cursor.context, Some(&canonical))?;
                let metadata = match std::fs::metadata(&canonical) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io_error(error)),
                };
                if metadata.is_dir() {
                    if cursor
                        .seen_directories
                        .insert(SourceStamp::of(&metadata).identity)
                    {
                        cursor
                            .directories
                            .push(std::fs::read_dir(&path).map_err(io_error)?);
                    }
                    continue;
                }
                if !metadata.is_file() || path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let stem = path.file_stem().expect("source filename").to_string_lossy();
                let Some(id) = Self::requested_location(&stem, &cursor.wanted) else {
                    continue;
                };
                if cursor.found.insert(id.to_owned()) {
                    let stamp = SourceStamp::of(&metadata);
                    cursor.pending.push_back(json!({"session_id":id,"status":"ok","path":path.to_string_lossy().as_ref(),"revision":stamp.revision()}));
                    updates.push((id.to_owned(), path));
                }
                continue;
            }
            for id in &cursor.ids {
                if !cursor.found.contains(id) {
                    cursor.pending.push_back(json!({"session_id":id,"status":if cursor.exhausted {"incomplete"} else {"missing"},"path":null,"revision":null}));
                }
            }
        }
        self.remember_locations(&updates, &cursor.context);
        let complete = cursor.finished && cursor.pending.is_empty();
        let data = json!({"kind":"located","sessions":output});
        if complete {
            return Ok((
                data,
                None,
                cursor
                    .exhausted
                    .then(|| "bounded location incomplete".to_owned()),
            ));
        }
        let mut state = self.state.lock().expect("snapshot state");
        Self::prune(&mut state);
        if state.locates.len() >= self.lease_cap(&cursor.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "location cursor admission exhausted",
            ));
        }
        self.admit_memory(&mut state, &cursor.context, cursor.accounted_bytes())?;
        cursor.expires = (now_ms() + self.config.ttl).min(cursor.limits.deadline_unix_ms);
        state.locates.insert(token.to_owned(), cursor);
        Ok((
            data,
            Some(token.to_owned()),
            Some("location page incomplete".to_owned()),
        ))
    }

    fn resolve(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bound = limits(request)?;
        let ids = request
            .get("session_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing session_ids"))?;
        let roots = request
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing roots"))?;
        let mut wanted = HashSet::new();
        for id in ids.iter() {
            wanted.insert(
                id.as_str()
                    .ok_or_else(|| invalid("invalid session id"))?
                    .to_owned(),
            );
        }
        let mut stack = Vec::new();
        for root in roots.iter() {
            let path = std::fs::canonicalize(root.as_str().ok_or_else(|| invalid("invalid root"))?)
                .map_err(io_error)?;
            self.authority(context, Some(&path))?;
            stack.push(PathBuf::from(root.as_str().expect("validated root")));
        }
        let mut found = HashMap::new();
        let mut complete = true;
        let mut identities = HashSet::new();
        let mut directories = HashSet::new();
        'walk: while let Some(path) = stack.pop() {
            let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
            self.authority(context, Some(&canonical))?;
            let root_metadata = std::fs::metadata(&canonical).map_err(io_error)?;
            if root_metadata.is_dir()
                && !directories.insert(SourceStamp::of(&root_metadata).identity)
            {
                continue;
            }
            let paths: Box<dyn Iterator<Item = Result<PathBuf, std::io::Error>>> =
                if std::fs::metadata(&path).map_err(io_error)?.is_file() {
                    Box::new(std::iter::once(Ok(path)))
                } else {
                    Box::new(
                        std::fs::read_dir(path)
                            .map_err(io_error)?
                            .map(|entry| entry.map(|entry| entry.path())),
                    )
                };
            for path in paths {
                cancel.check(bound.deadline_unix_ms)?;
                if usage[17] as usize >= bound.max_discovery_entries
                    || identities.len() >= bound.max_sources
                {
                    complete = false;
                    break 'walk;
                }
                usage[17] += 1;
                let path = path.map_err(io_error)?;
                let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
                self.authority(context, Some(&canonical))?;
                let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let stem = path.file_stem().expect("source filename").to_string_lossy();
                let Some(id) = wanted
                    .iter()
                    .find(|id| stem.as_ref() == id.as_str() || stem.ends_with(&format!("-{id}")))
                else {
                    continue;
                };
                let stamp = SourceStamp::of(&metadata);
                if identities.insert(stamp.identity) {
                    found.insert(id.clone(), path);
                }
            }
        }
        let token = self.token("resolution");
        let cursor = ResolutionCursor {
            claimant: str_field(context, "claimant")?.to_owned(),
            context: context.clone(),
            request: request.clone(),
            ids: ids
                .iter()
                .map(|id| id.as_str().expect("validated id").to_owned())
                .collect(),
            paths: found,
            sessions: Vec::new(),
            next: 0,
            pending: None,
            remaining: bound,
            complete_scan: complete,
            expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
        };
        self.resolve_step(&token, cursor, cancel, usage)
    }

    fn resolve_step(
        &self,
        token: &str,
        mut cursor: ResolutionCursor,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(cursor.remaining.deadline_unix_ms)?;
        {
            let mut state = self.state.lock().expect("snapshot state");
            for session in &mut cursor.sessions {
                if session["status"].as_str() != Some("ok") {
                    continue;
                }
                let handle = &session["description"]["handle"];
                let lease = self.lease(&state, handle, &cursor.context)?;
                let expires = lease.expires.max(
                    cursor
                        .remaining
                        .deadline_unix_ms
                        .min(lease.absolute_deadline),
                );
                state
                    .leases
                    .get_mut(str_field(handle, "lease_id")?)
                    .expect("validated lease")
                    .expires = expires;
                session["description"].insert("lease_expires_unix_ms", json!(expires));
            }
        }
        while cursor.next < cursor.ids.len() {
            let id = &cursor.ids[cursor.next];
            let Some(path) = cursor.paths.get(id) else {
                cursor.sessions.push(json!({"session_id":id,"status":if cursor.complete_scan {"missing"} else {"incomplete"},"description":null}));
                cursor.next += 1;
                continue;
            };
            let before_read = usage[1];
            let before_events = usage[3];
            let outcome = if let Some(pending) = &cursor.pending {
                let waiter = self
                    .state
                    .lock()
                    .expect("snapshot state")
                    .waiters
                    .get(pending)
                    .cloned()
                    .ok_or_else(|| {
                        SnapshotError::new(Status::StaleCursor, "resolution reservation expired")
                    })?;
                self.authority(&cursor.context, Some(path))?;
                if let Some(current) = self
                    .state
                    .lock()
                    .expect("snapshot state")
                    .waiters
                    .get_mut(pending)
                {
                    current.context = cursor.context.clone();
                }
                self.advance(pending, waiter, cancel, usage)?
            } else {
                let mut acquire = cursor.request.clone();
                acquire.insert("operation", json!("acquire"));
                acquire.insert("path", json!(path.to_string_lossy().as_ref()));
                let bound = cursor.remaining;
                acquire.insert("limits", json!({"max_read_bytes":bound.max_read_bytes,"max_events":bound.max_events,"max_items":bound.max_items,"max_output_bytes":bound.max_output_bytes,"max_discovery_entries":bound.max_discovery_entries,"max_sources":bound.max_sources}));
                self.acquire(&acquire, &cursor.context, cancel, usage)?
            };
            cursor.remaining.max_read_bytes = cursor
                .remaining
                .max_read_bytes
                .saturating_sub((usage[1] - before_read) as usize);
            cursor.remaining.max_events = cursor
                .remaining
                .max_events
                .saturating_sub((usage[3] - before_events) as usize);
            cursor.pending = outcome.1;
            if cursor.pending.is_none() {
                let handle = &outcome.0["description"]["handle"];
                let (resolved, description) = self.pin_scope_for_work(
                    handle,
                    &cursor.context,
                    cursor.remaining.deadline_unix_ms,
                )?;
                if resolved.session_id != *id {
                    self.state
                        .lock()
                        .expect("snapshot state")
                        .leases
                        .remove(str_field(handle, "lease_id")?);
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "candidate source session differs from requested identity",
                    ));
                }
                cursor
                    .sessions
                    .push(json!({"session_id":id,"status":"ok","description":description}));
                cursor.next += 1;
            }
            break;
        }
        if cursor.next == cursor.ids.len() {
            let data = json!({"kind":"resolved","sessions":cursor.sessions});
            let bytes = sonic_rs::to_vec(&data)
                .map_err(|error| invalid(error.to_string()))?
                .len();
            if bytes > cursor.remaining.max_output_bytes {
                return Err(SnapshotError::new(
                    Status::OutputLimit,
                    "resolution output budget exhausted",
                ));
            }
            return Ok((
                data,
                None,
                (!cursor.complete_scan).then(|| "bounded resolution incomplete".to_owned()),
            ));
        }
        let mut sessions = cursor.sessions.clone();
        for id in &cursor.ids[cursor.next..] {
            sessions.push(json!({"session_id":id,"status":"incomplete","description":null}));
        }
        let data = json!({"kind":"resolved","sessions":sessions});
        let bytes = sonic_rs::to_vec(&data)
            .map_err(|error| invalid(error.to_string()))?
            .len();
        if bytes > cursor.remaining.max_output_bytes {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "resolution output budget exhausted",
            ));
        }
        cursor.remaining.max_output_bytes -= bytes;
        let mut state = self.state.lock().expect("snapshot state");
        if state.resolutions.len() >= self.lease_cap(&cursor.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "resolution cursor admission exhausted",
            ));
        }
        self.admit_memory(&mut state, &cursor.context, bytes * 2)?;
        cursor.expires = (now_ms() + self.config.ttl).min(cursor.remaining.deadline_unix_ms);
        state.resolutions.insert(token.to_owned(), cursor);
        Ok((
            data,
            Some(token.to_owned()),
            Some("resolution preparation incomplete".to_owned()),
        ))
    }
}

#[cfg(test)]
#[path = "snapshot_regressions.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Source {
        directory: PathBuf,
        path: PathBuf,
    }

    impl Source {
        fn new(contents: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "cc-snapshot-{}-{}-{}",
                std::process::id(),
                now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("s.jsonl");
            std::fs::write(&path, contents).unwrap();
            Self { directory, path }
        }

        fn append(&self, value: &str) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap()
                .write_all(value.as_bytes())
                .unwrap();
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    fn user(id: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{id}","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"hello"}}}}"#
        )
    }

    fn context(claimant: &str) -> Value {
        json!({"claimant":claimant,"admission":"hook","authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},"registry_generation":crate::toolcall::ToolRegistrySnapshot::from_specs(HashMap::new()).fingerprint()})
    }

    fn store() -> NativeStore {
        NativeStore::new(&json!({"max_read_bytes_per_step":128,"max_events_per_step":2,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
    }

    fn acquire(path: &Path) -> Value {
        json!({"schema":SCHEMA,"id":"request","operation":"acquire","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},
            "deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_events":1000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
    }

    fn finish(store: &NativeStore, mut response: Value, context: &Value) -> Value {
        for _ in 0..100 {
            if response["status"].as_str() != Some("incomplete") {
                return response;
            }
            let cursor = response["cursor"].as_str().expect("resumable preparation");
            response = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                context,
                &Cancellation::default(),
            );
        }
        panic!("preparation did not finish");
    }

    fn handle(response: &Value) -> &Value {
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        &response["data"]["description"]["handle"]
    }

    #[test]
    fn callers_share_a_load_and_cancellation_detaches_only_one() {
        let source = Source::new(&format!("{}\n{}\n{}\n", user("a"), user("b"), user("c")));
        let store = store();
        let first = store.request(
            &acquire(&source.path),
            &context("a"),
            &Cancellation::default(),
        );
        let second = store.request(
            &acquire(&source.path),
            &context("b"),
            &Cancellation::default(),
        );
        assert_eq!(
            first["data"]["reservation"]["load_id"].as_str(),
            second["data"]["reservation"]["load_id"].as_str()
        );
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let result = store.request(
            &json!({"schema":SCHEMA,"id":"cancel","operation":"resume","cursor":first["cursor"]}),
            &context("a"),
            &cancelled,
        );
        assert_eq!(result["status"].as_str(), Some("cancelled"));
        let complete = finish(&store, second, &context("b"));
        assert_eq!(
            store
                .pin(handle(&complete), &context("b"))
                .unwrap()
                .event_count,
            3
        );
        let state = store.state.lock().unwrap();
        assert_eq!(state.counters[4], 1);
        assert_eq!(state.counters[8], 1);
        assert_eq!(
            state.counters[1],
            std::fs::metadata(&source.path).unwrap().len() + 128
        );
    }

    #[test]
    fn append_shares_committed_chunks_and_keeps_old_generation_immutable() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prior = store.pin(handle(&first), &owner).unwrap();
        let before = store.state.lock().unwrap().counters[1];
        let added = format!("{}\n", user("b"));
        source.append(&added);
        let second = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let current = store.pin(handle(&second), &owner).unwrap();
        assert_eq!(prior.event_count, 1);
        assert_eq!(current.event_count, 2);
        assert!(Arc::ptr_eq(&prior.chunks[0], &current.chunks[0]));
        assert_eq!(
            store.state.lock().unwrap().counters[1] - before,
            prior.fence.len() as u64 + added.len() as u64 + 128
        );
    }

    #[test]
    fn provisional_tail_is_replaced_without_duplicate_events() {
        let source = Source::new(&user("a"));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prior = store.pin(handle(&first), &owner).unwrap();
        assert!(prior.provisional_tail);
        source.append(&format!("\n{}\n", user("b")));
        let second = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let current = store.pin(handle(&second), &owner).unwrap();
        assert_eq!(current.event_count, 2);
        assert!(!current.provisional_tail);
        assert_eq!(current.entry(0).meta().unwrap().uuid, "a");
        assert_eq!(current.entry(1).meta().unwrap().uuid, "b");
        assert_eq!(prior.event_count, 1);
    }

    #[test]
    fn hardlink_aliases_reuse_the_same_generation_and_claimants_are_isolated() {
        let source = Source::new(&format!("{}\n", user("a")));
        let alias = source.directory.join("alias.jsonl");
        std::fs::hard_link(&source.path, &alias).unwrap();
        let store = store();
        let first = finish(
            &store,
            store.request(
                &acquire(&source.path),
                &context("a"),
                &Cancellation::default(),
            ),
            &context("a"),
        );
        let second = finish(
            &store,
            store.request(&acquire(&alias), &context("b"), &Cancellation::default()),
            &context("b"),
        );
        assert_eq!(
            handle(&first)["generation"].as_str(),
            handle(&second)["generation"].as_str()
        );
        assert_eq!(
            store.pin(handle(&first), &context("b")).unwrap_err().status,
            Status::StaleHandle
        );
        assert_eq!(store.state.lock().unwrap().counters[4], 1);
    }

    #[test]
    fn replacement_and_same_size_rewrite_do_not_reuse_generation() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let old = store.pin(handle(&first), &owner).unwrap();
        let replacement = source.directory.join("replacement");
        std::fs::write(&replacement, format!("{}\n", user("b"))).unwrap();
        std::fs::rename(replacement, &source.path).unwrap();
        let second = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let new = store.pin(handle(&second), &owner).unwrap();
        assert_ne!(old.stamp.identity, new.stamp.identity);
        assert_eq!(new.entry(0).meta().unwrap().uuid, "b");
        assert_eq!(old.entry(0).meta().unwrap().uuid, "a");
    }

    #[test]
    fn expired_and_restarted_handles_are_stale() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let response = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let lease = handle(&response);
        store
            .state
            .lock()
            .unwrap()
            .leases
            .get_mut(lease["lease_id"].as_str().unwrap())
            .unwrap()
            .expires = now_ms() - 1;
        assert_eq!(
            store.pin(lease, &owner).unwrap_err().status,
            Status::StaleHandle
        );
        let restarted = NativeStore::new(&json!({})).unwrap();
        assert_eq!(
            restarted.pin(lease, &owner).unwrap_err().status,
            Status::StaleHandle
        );
    }

    #[test]
    fn escaped_borrowed_entries_remain_accounted_after_release() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let response = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        let escaped = Arc::clone(&snapshot.chunks[0].entries);
        drop(snapshot);
        let mut state = store.state.lock().unwrap();
        state.leases.clear();
        state.latest.clear();
        NativeStore::prune(&mut state);
        assert!(
            number(
                &NativeStore::gauges(&state),
                "retained_entry_capacity_bytes"
            )
            .unwrap()
                > 0
        );
        drop(escaped);
        NativeStore::prune(&mut state);
        assert_eq!(
            number(
                &NativeStore::gauges(&state),
                "retained_entry_capacity_bytes"
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn budgets_and_authority_fail_explicitly() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let mut request = acquire(&source.path);
        request["limits"].insert("max_read_bytes", json!(10));
        let result = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(result["status"].as_str(), Some("source_limit"));
        let mut denied = owner.clone();
        denied["authority"].insert(
            "effective_uid",
            json!((unsafe { libc::geteuid() } as u64 + 1).to_string()),
        );
        let result = store.request(&acquire(&source.path), &denied, &Cancellation::default());
        assert_eq!(result["status"].as_str(), Some("permission_denied"));
    }
    #[test]
    fn discovery_restats_files_when_directory_membership_is_unchanged() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"discover","operation":"discover","roots":[source.directory.to_string_lossy().as_ref()],"checkpoint":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["status"].as_str(), Some("ok"));
        let revision = first["data"]["entries"][0]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        request.insert("checkpoint", first["data"]["checkpoint"].clone());
        let directory_stamp = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
        source.append(&format!("{}\n", user("b")));
        let after = SourceStamp::of(&std::fs::metadata(&source.directory).unwrap());
        assert_eq!(directory_stamp.mtime_ns, after.mtime_ns);
        let second = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(second["status"].as_str(), Some("ok"));
        assert_ne!(
            second["data"]["entries"][0]["revision"].as_str(),
            Some(revision.as_str())
        );
        let acquired = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(store.pin(handle(&acquired), &owner).unwrap().event_count, 2);
    }

    #[test]
    fn partial_resolution_never_declares_missing() {
        let source = Source::new(&format!("{}\n", user("a")));
        std::fs::write(
            source.directory.join("other.jsonl"),
            format!("{}\n", user("b")),
        )
        .unwrap();
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let mut bounds = template["limits"].clone();
        bounds.insert("max_discovery_entries", json!(1));
        let request = json!({"schema":SCHEMA,"id":"resolve","operation":"resolve","session_ids":["absent"],"roots":[source.directory.to_string_lossy().as_ref()],"classifier":{"id":"native","version":"1"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":bounds});
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("incomplete"));
        assert_eq!(
            response["data"]["sessions"][0]["status"].as_str(),
            Some("incomplete")
        );
    }

    #[test]
    fn bulk_location_reuses_validated_paths_without_leases() {
        let source = Source::new(&format!("{}\n", user("a")));
        let ids: Vec<_> = (0..913)
            .map(|index| format!("session-{index:04}"))
            .collect();
        for id in &ids {
            std::fs::write(source.directory.join(format!("{id}.jsonl")), b"").unwrap();
        }
        let store = NativeStore::new(&json!({
            "max_events_per_step":2048,
            "max_retained_bytes":32*1024*1024,
            "reserved_hook_accounted_bytes":4096
        }))
        .unwrap();
        let owner = context("a");
        let template = acquire(&source.path);
        let mut bounds = template["limits"].clone();
        bounds.insert("max_discovery_entries", json!(2048));
        bounds.insert("max_sources", json!(1024));
        bounds.insert("max_items", json!(1024));
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":bounds});
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(first["data"]["sessions"].as_array().unwrap().len(), 913);
        assert!(
            first["usage"]["discovery_entries_examined"]
                .as_u64()
                .unwrap()
                <= 914
        );
        assert_eq!(first["usage"]["source_opens"].as_u64(), Some(0));
        assert_eq!(store.state.lock().unwrap().leases.len(), 0);
        let warm = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm["data"]["sessions"].as_array().unwrap().len(), 913);
        assert_eq!(
            warm["usage"]["discovery_entries_examined"].as_u64(),
            Some(0)
        );
        assert_eq!(store.state.lock().unwrap().leases.len(), 0);
    }

    #[test]
    fn location_budget_never_claims_an_unseen_session_missing() {
        let source = Source::new(&format!("{}\n", user("a")));
        for index in 0..40 {
            std::fs::write(source.directory.join(format!("id-{index:02}.jsonl")), b"").unwrap();
        }
        let store = NativeStore::new(&json!({
            "max_events_per_step":4,
            "max_retained_bytes":32*1024*1024,
            "reserved_hook_accounted_bytes":4096
        }))
        .unwrap();
        let owner = context("a");
        let template = acquire(&source.path);
        let mut bounds = template["limits"].clone();
        bounds.insert("max_discovery_entries", json!(12));
        bounds.insert("max_sources", json!(1024));
        bounds.insert("max_items", json!(1024));
        let request = json!({"schema":SCHEMA,"id":"locate-bounded","operation":"locate","session_ids":["absent"],"roots":[source.directory.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":bounds});
        let mut reply = store.request(&request, &owner, &Cancellation::default());
        let mut examined = 0;
        while let Some(cursor) = reply["cursor"].as_str() {
            examined += reply["usage"]["discovery_entries_examined"]
                .as_u64()
                .unwrap();
            reply = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
        }
        examined += reply["usage"]["discovery_entries_examined"]
            .as_u64()
            .unwrap();
        assert_eq!(examined, 12);
        assert_eq!(reply["status"].as_str(), Some("incomplete"));
        assert_eq!(
            reply["data"]["sessions"][0]["status"].as_str(),
            Some("incomplete")
        );
        assert_eq!(store.state.lock().unwrap().leases.len(), 0);
    }

    #[test]
    fn location_rechecks_changed_paths_and_does_not_cache_missing() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate-fresh","operation":"locate","session_ids":["s"],"roots":[source.directory.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["data"]["sessions"][0]["status"].as_str(), Some("ok"));
        let prior_revision = first["data"]["sessions"][0]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        std::fs::rename(&source.path, source.directory.join("moved.jsonl")).unwrap();
        let missing = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(
            missing["data"]["sessions"][0]["status"].as_str(),
            Some("missing")
        );
        std::fs::write(&source.path, format!("{}\n", user("b"))).unwrap();
        let present = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(
            present["data"]["sessions"][0]["status"].as_str(),
            Some("ok")
        );
        assert_ne!(
            present["data"]["sessions"][0]["revision"].as_str(),
            Some(prior_revision.as_str())
        );
        assert_eq!(store.state.lock().unwrap().leases.len(), 0);
    }

    #[test]
    fn custom_classifier_is_charged_before_python_event_views() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let _base = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        store
            .register_classifier(
                "test",
                "1",
                Arc::new(move |_, range| {
                    callback_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(vec![false; range.len()])
                }),
            )
            .unwrap();
        let mut request = acquire(&source.path);
        request.insert("classifier", json!({"id":"test","version":"1"}));
        request["limits"].insert("max_read_bytes", json!(2));
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("incomplete"));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn pinned_prefix_finishes_while_source_appends_between_every_stage() {
        let source = Source::new(&format!("{}\n", user("initial")));
        let store = store();
        let owner = context("a");
        let pinned_size = std::fs::metadata(&source.path).unwrap().len();
        let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        let mut steps = 0;
        while response["status"].as_str() == Some("incomplete") {
            assert!(
                steps < 20,
                "cold preparation must make progress during append"
            );
            source.append(&format!("{}\n", user(&format!("later-{steps}"))));
            let cursor = response["cursor"].as_str().unwrap().to_owned();
            response = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
            steps += 1;
        }
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        assert_eq!(snapshot.stamp.size, pinned_size);
        assert_eq!(snapshot.event_count, 1);
        assert_eq!(snapshot.entry(0).meta().unwrap().uuid, "initial");
        let state = store.state.lock().unwrap();
        assert_eq!(state.counters[4], 1);
        assert_eq!(state.counters[1], pinned_size + 128);
    }
    fn graph_request(
        root: &Value,
        query: Value,
        attachments: Vec<String>,
        selectors: Value,
    ) -> Value {
        let template = acquire(Path::new("/unused"));
        json!({"schema":SCHEMA,"id":"graph-query","operation":"query","view":{"handle":handle(root),"classifier":{"id":"native","version":"1"},"selectors":selectors,"attachments":attachments},"query":query,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]})
    }

    fn graph_records(store: &NativeStore, request: &Value, owner: &Value) -> Vec<Value> {
        let mut response = store.request(request, owner, &Cancellation::default());
        let mut records = Vec::new();
        for _ in 0..150 {
            if let Some(items) = response["data"]
                .get("records_json")
                .and_then(Value::as_array)
            {
                for item in items.iter() {
                    records.push(sonic_rs::from_str(item.as_str().unwrap()).unwrap());
                }
            }
            if response["status"].as_str() == Some("ok") {
                return records;
            }
            assert_eq!(
                response["status"].as_str(),
                Some("incomplete"),
                "{response:?}"
            );
            let cursor = response["cursor"]
                .as_str()
                .expect("bounded graph continuation")
                .to_owned();
            response = store.request(
                &json!({"schema":SCHEMA,"id":"graph-next","operation":"resume","cursor":cursor}),
                owner,
                &Cancellation::default(),
            );
        }
        panic!("graph did not finish");
    }

    #[test]
    fn classifier_continuation_coalesces_without_repeating_successful_batches() {
        let source = Source::new(
            &(0..5)
                .map(|index| format!("{}\n", user(&index.to_string())))
                .collect::<String>(),
        );
        let store = store();
        let a = context("a");
        let b = context("b");
        let _native = finish(
            &store,
            store.request(&acquire(&source.path), &a, &Cancellation::default()),
            &a,
        );
        let batches = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&batches);
        store
            .register_classifier(
                "staged",
                "1",
                Arc::new(move |_, range| {
                    recorded.lock().unwrap().push(range.clone());
                    Ok(vec![true; range.len()])
                }),
            )
            .unwrap();
        let mut request = acquire(&source.path);
        request.insert("classifier", json!({"id":"staged","version":"1"}));
        let first = store.request(&request, &a, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"));
        let second = store.request(&request, &b, &Cancellation::default());
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let failed=store.request(&json!({"schema":SCHEMA,"id":"cancel-classifier","operation":"resume","cursor":first["cursor"]}),&a,&cancelled);
        assert_eq!(failed["status"].as_str(), Some("cancelled"));
        let completed = finish(&store, second, &b);
        assert_eq!(store.pin(handle(&completed), &b).unwrap().event_count, 5);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..4, 4..5]);
    }

    #[test]
    fn graph_walk_is_sorted_dfs_then_attachments_and_deduplicates_physical_aliases() {
        let source = Source::new(&format!("{}\n", user("root")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        let a = children.join("agent-a.jsonl");
        let b = children.join("agent-b.jsonl");
        std::fs::write(&a, format!("{}\n", user("a"))).unwrap();
        std::fs::write(&b, format!("{}\n", user("b"))).unwrap();
        let grandchildren = children.join("agent-a/subagents");
        std::fs::create_dir_all(&grandchildren).unwrap();
        std::fs::write(
            grandchildren.join("agent-z.jsonl"),
            format!("{}\n", user("z")),
        )
        .unwrap();
        let attachment = source.directory.join("external.jsonl");
        std::fs::write(&attachment, format!("{}\n", user("external"))).unwrap();
        let alias = source.directory.join("alias.jsonl");
        std::fs::hard_link(&b, &alias).unwrap();
        let store=NativeStore::new(&json!({"max_read_bytes_per_step":128,"max_events_per_step":2,"max_items_per_page":2,"max_leases":16,"reserved_hook_leases":1,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let records = graph_records(
            &store,
            &graph_request(
                &root,
                json!({"kind":"sidechain_membership","order":"forward"}),
                vec![
                    alias.to_string_lossy().into_owned(),
                    attachment.to_string_lossy().into_owned(),
                ],
                json!([]),
            ),
            &owner,
        );
        let names: Vec<_> = records
            .iter()
            .map(|record| {
                Path::new(record["path"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "agent-a.jsonl",
                "agent-z.jsonl",
                "agent-b.jsonl",
                "external.jsonl"
            ]
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record["depth"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 1, 1]
        );
        assert!(records[3]["spawned_by"].is_null());
        for record in records {
            assert!(store.pin(&record["description"]["handle"], &owner).is_ok());
        }
    }

    #[test]
    fn direct_sidechains_skip_unselected_sources_descendants_and_attachments() {
        let source = Source::new(&format!("{}\n{}\n", user("before"), user("current")));
        let children = source.directory.join("s/subagents");
        let descendants = children.join("agent-chosen/subagents");
        std::fs::create_dir_all(&descendants).unwrap();
        std::fs::write(
            children.join("agent-chosen.jsonl"),
            format!("{}\n{}\n", user("child-first"), user("child-last")),
        )
        .unwrap();
        std::fs::write(children.join("agent-unselected.jsonl"), "not JSON\n").unwrap();
        std::fs::write(descendants.join("agent-broken.jsonl"), "not JSON\n").unwrap();
        let attachment = source.directory.join("broken.jsonl");
        std::fs::write(&attachment, "not JSON\n").unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let before = store.state.lock().unwrap().counters[0];
        let records = graph_records(
            &store,
            &graph_request(
                &root,
                json!({"kind":"direct_sidechains","order":"forward","dispatch_ids":["chosen"]}),
                vec![attachment.to_string_lossy().into_owned()],
                json!([{"kind":"current_turn"}]),
            ),
            &owner,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["spawned_by"].as_str(), Some("chosen"));
        assert_eq!(records[0]["depth"].as_u64(), Some(1));
        assert_eq!(
            records[0]["description"]["classifier"]["id"].as_str(),
            Some("native")
        );
        assert_eq!(
            store
                .pin(&records[0]["description"]["handle"], &owner)
                .unwrap()
                .event_count,
            2
        );
        assert_eq!(store.state.lock().unwrap().counters[0] - before, 1);
    }

    #[test]
    fn empty_direct_sidechains_do_not_inspect_child_sources() {
        let source = Source::new(&format!("{}\n", user("a")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(children.join("agent-broken.jsonl"), "not JSON\n").unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let before = store.state.lock().unwrap().counters;
        let records = graph_records(
            &store,
            &graph_request(
                &root,
                json!({"kind":"direct_sidechains","order":"forward","dispatch_ids":[]}),
                vec![source
                    .directory
                    .join("absent.jsonl")
                    .to_string_lossy()
                    .into_owned()],
                json!([]),
            ),
            &owner,
        );
        assert!(records.is_empty());
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[0], before[0]);
        assert_eq!(after[17], before[17]);
        assert_eq!(store.state.lock().unwrap().leases.len(), 1);
    }

    #[test]
    fn direct_sidechains_preserve_dispatch_aliases_with_shared_source_chunks() {
        let source = Source::new(&format!("{}\n", user("a")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        let first = children.join("agent-a.jsonl");
        std::fs::write(&first, format!("{}\n", user("child"))).unwrap();
        std::fs::hard_link(&first, children.join("agent-b.jsonl")).unwrap();
        let store = NativeStore::new(&json!({"max_items_per_page":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let records = graph_records(
            &store,
            &graph_request(
                &root,
                json!({"kind":"direct_sidechains","order":"forward","dispatch_ids":["b","a"]}),
                Vec::new(),
                json!([]),
            ),
            &owner,
        );
        assert_eq!(
            records
                .iter()
                .map(|record| record["spawned_by"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let first = store
            .pin(&records[0]["description"]["handle"], &owner)
            .unwrap();
        let second = store
            .pin(&records[1]["description"]["handle"], &owner)
            .unwrap();
        assert!(Arc::ptr_eq(&first.chunks[0], &second.chunks[0]));
        assert_ne!(
            records[0]["description"]["handle"]["lease_id"],
            records[1]["description"]["handle"]["lease_id"]
        );
    }

    #[test]
    fn direct_sidechain_discovery_limit_cannot_report_missing() {
        let source = Source::new(&format!("{}\n", user("a")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(children.join("agent-unselected.jsonl"), "not JSON\n").unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut request = graph_request(
            &root,
            json!({"kind":"direct_sidechains","order":"forward","dispatch_ids":["missing"]}),
            Vec::new(),
            json!([]),
        );
        request["limits"].insert("max_discovery_entries", json!(0));
        let result = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(result["status"].as_str(), Some("incomplete"));
        assert_eq!(result["complete"].as_bool(), Some(false));
        assert!(result["cursor"].is_null());
        assert!(result["data"].is_null());
        assert_eq!(store.state.lock().unwrap().leases.len(), 1);
    }

    #[test]
    fn direct_sidechain_failure_releases_prepared_members() {
        let source = Source::new(&format!("{}\n", user("a")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(
            children.join("agent-a.jsonl"),
            format!("{}\n", user("child")),
        )
        .unwrap();
        std::fs::write(children.join("agent-b.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let request = graph_request(
            &root,
            json!({"kind":"direct_sidechains","order":"forward","dispatch_ids":["a","b"]}),
            Vec::new(),
            json!([]),
        );
        let result = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(result["status"].as_str(), Some("parse_error"));
        assert_eq!(result["complete"].as_bool(), Some(false));
        let state = store.state.lock().unwrap();
        assert_eq!(state.leases.len(), 1);
        assert!(state.graphs.is_empty());
    }

    #[test]
    fn graph_predicates_use_whole_children_and_local_queries_ignore_attachments() {
        let source = Source::new(&format!("{}\n{}\n", user("root-first"), user("root-last")));
        let child = source.directory.join("external.jsonl");
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"child.rs"}}]}}"#;
        std::fs::write(
            &child,
            format!("{}\n{tool}\n{}\n", user("child-first"), user("child-last")),
        )
        .unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let attachments = vec![child.to_string_lossy().into_owned()];
        let local = store.request(
            &graph_request(
                &root,
                json!({"kind":"has_read","pattern":"child.rs","subagents":false}),
                attachments.clone(),
                json!([{"kind":"current_turn"}]),
            ),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(local["status"].as_str(), Some("ok"));
        assert_eq!(local["data"]["value"].as_bool(), Some(false));
        let request = graph_request(
            &root,
            json!({"kind":"has_read","pattern":"child.rs","subagents":true}),
            attachments,
            json!([{"kind":"current_turn"}]),
        );
        let result = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(result["status"].as_str(), Some("ok"));
        assert_eq!(result["data"]["value"].as_bool(), Some(true));
    }

    #[test]
    fn symlink_file_roots_resolve_and_directory_cycles_terminate() {
        let source = Source::new(&format!("{}\n", user("a")));
        let physical = source.directory.join("physical.jsonl");
        std::fs::rename(&source.path, &physical).unwrap();
        std::os::unix::fs::symlink(&physical, &source.path).unwrap();
        std::os::unix::fs::symlink(&source.directory, source.directory.join("cycle")).unwrap();
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let resolve = json!({"schema":SCHEMA,"id":"resolve-link","operation":"resolve","session_ids":["s"],"roots":[source.path.to_string_lossy().as_ref()],"classifier":{"id":"native","version":"1"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let resolved = finish(
            &store,
            store.request(&resolve, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(resolved["status"].as_str(), Some("ok"));
        assert_eq!(
            resolved["data"]["sessions"][0]["status"].as_str(),
            Some("ok")
        );
        let discover = json!({"schema":SCHEMA,"id":"discover-cycle","operation":"discover","roots":[source.directory.to_string_lossy().as_ref()],"checkpoint":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let discovered = finish(
            &store,
            store.request(&discover, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(discovered["status"].as_str(), Some("ok"));
        let state = store.state.lock().unwrap();
        assert!(state.counters[17] < 10);
        assert_eq!(
            state.checkpoints.values().next().unwrap().inventory.len(),
            1
        );
    }

    #[test]
    fn incomplete_graph_never_returns_a_false_predicate() {
        let source = Source::new(&format!("{}\n", user("root")));
        let children = source.directory.join("s/subagents");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(children.join("agent-a.jsonl"), format!("{}\n", user("a"))).unwrap();
        std::fs::write(children.join("agent-b.jsonl"), format!("{}\n", user("b"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut request = graph_request(
            &root,
            json!({"kind":"has_tool","pattern":"Read","subagents":true}),
            Vec::new(),
            json!([]),
        );
        request["limits"].insert("max_discovery_entries", json!(1));
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("incomplete"));
        assert!(response["data"].is_null());
        assert!(response["cursor"].is_null());
    }
    fn register_changed_registry(store: &NativeStore) -> String {
        store
            .register_tool_registry(
                &json!([{"name":"fetch","behaves_like":"Read","span_edit":null}]),
                &context("a"),
            )
            .unwrap()
    }

    #[test]
    fn resume_registry_and_admission_changes_do_not_consume_waiter() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let owner = context("a");
        let response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        let cursor = response["cursor"].as_str().unwrap().to_owned();
        let mut changed = owner.clone();
        changed.insert(
            "registry_generation",
            json!(register_changed_registry(&store)),
        );
        let resume =
            json!({"schema":SCHEMA,"id":"changed-resume","operation":"resume","cursor":cursor});
        let before = store.state.lock().unwrap().counters[1];
        assert_eq!(
            store.request(&resume, &changed, &Cancellation::default())["status"].as_str(),
            Some("stale_cursor")
        );
        let mut changed_role = owner.clone();
        changed_role.insert("admission", json!("review"));
        assert_eq!(
            store.request(&resume, &changed_role, &Cancellation::default())["status"].as_str(),
            Some("stale_cursor")
        );
        assert_eq!(store.state.lock().unwrap().counters[1], before);
        assert!(store.state.lock().unwrap().waiters.contains_key(&cursor));
        let completed = finish(
            &store,
            store.request(&resume, &owner, &Cancellation::default()),
            &owner,
        );
        assert!(store.pin(handle(&completed), &owner).is_ok());
    }

    #[test]
    fn resolution_registry_change_is_rejected_before_the_next_source_stage() {
        let source = Source::new(&format!("{}\n", user("first")));
        let other = source.directory.join("t.jsonl");
        std::fs::write(
            &other,
            format!(
                "{}\n",
                user("second").replace("\"sessionId\":\"s\"", "\"sessionId\":\"t\"")
            ),
        )
        .unwrap();
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"resolve-stages","operation":"resolve","session_ids":["s","t"],"roots":[source.directory.to_string_lossy().as_ref()],"classifier":{"id":"native","version":"1"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let started = store.request(&request, &owner, &Cancellation::default());
        let cursor = started["cursor"].as_str().unwrap().to_owned();
        let mut changed = owner.clone();
        changed.insert(
            "registry_generation",
            json!(register_changed_registry(&store)),
        );
        let resume =
            json!({"schema":SCHEMA,"id":"resume-resolve","operation":"resume","cursor":cursor});
        assert_eq!(
            store.request(&resume, &changed, &Cancellation::default())["status"].as_str(),
            Some("stale_cursor")
        );
        assert!(store
            .state
            .lock()
            .unwrap()
            .resolutions
            .contains_key(&cursor));
        let completed = finish(
            &store,
            store.request(&resume, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(completed["status"].as_str(), Some("ok"));
        assert_eq!(completed["data"]["sessions"].as_array().unwrap().len(), 2);
        for session in completed["data"]["sessions"].as_array().unwrap().iter() {
            assert_eq!(session["status"].as_str(), Some("ok"));
        }
    }

    #[test]
    fn registry_change_reindexes_cached_chunks_without_reading_source_again() {
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"mcp__example__fetch","input":{"file_path":"file.rs"}}]}}"#;
        let source = Source::new(&format!("{}\n{tool}\n", user("a")));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let before = store.state.lock().unwrap().counters[1];
        let mut changed = owner.clone();
        changed.insert(
            "registry_generation",
            json!(register_changed_registry(&store)),
        );
        let second = finish(
            &store,
            store.request(&acquire(&source.path), &changed, &Cancellation::default()),
            &changed,
        );
        let prior = store.pin(handle(&first), &owner).unwrap();
        let next = store.pin(handle(&second), &changed).unwrap();
        assert_eq!(store.state.lock().unwrap().counters[1], before);
        assert!(Arc::ptr_eq(&prior.chunks[0], &next.chunks[0]));
        assert_ne!(prior.id, next.id);
        let old_query = graph_request(
            &first,
            json!({"kind":"has_tool","pattern":"Read","subagents":false}),
            Vec::new(),
            json!([]),
        );
        let new_query = graph_request(
            &second,
            json!({"kind":"has_tool","pattern":"Read","subagents":false}),
            Vec::new(),
            json!([]),
        );
        assert_eq!(
            store.request(&old_query, &owner, &Cancellation::default())["data"]["value"].as_bool(),
            Some(false)
        );
        assert_eq!(
            store.request(&new_query, &changed, &Cancellation::default())["data"]["value"]
                .as_bool(),
            Some(true)
        );
    }

    #[test]
    fn old_registry_cleanup_releases_only_undelivered_new_handles() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let second = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        let mut changed = owner.clone();
        changed.insert(
            "registry_generation",
            json!(register_changed_registry(&store)),
        );
        assert!(store.discard_response(&second, &changed).unwrap());
        assert!(store.pin(handle(&second), &owner).is_err());
        assert!(store.pin(handle(&first), &owner).is_ok());
        let describe=store.request(&json!({"schema":SCHEMA,"id":"describe","operation":"describe","handle":handle(&first)}),&owner,&Cancellation::default());
        assert!(!store.discard_response(&describe, &changed).unwrap());
        assert!(store.pin(handle(&first), &owner).is_ok());
        let release=store.request(&json!({"schema":SCHEMA,"id":"release-old","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle(&first)["lease_id"]}),&changed,&Cancellation::default());
        assert_eq!(release["data"]["released"].as_bool(), Some(true));
    }

    #[test]
    fn wrapped_domain_cursor_delivery_has_the_same_cleanup_identity() {
        let store = store();
        let owner = context("a");
        let cursor = format!("{}:0", store.owned_token());
        store.track_delivery(&json!({"cursor":cursor}), &owner, false);
        let wrapped =
            json!({"id":"outer","status":"incomplete","data":{"metadata":{}},"cursor":cursor});
        assert!(store.discard_response(&wrapped, &owner).unwrap());
        assert!(!store.discard_response(&wrapped, &owner).unwrap());
    }

    #[test]
    fn external_classifier_preparations_bind_execution_and_rotate_cursors() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let bounds = limits(&acquire(&source.path)).unwrap();
        let classifier = json!({"id":"application","version":"1"});
        let page = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        let cursor = page["cursor"].as_str().unwrap().to_owned();
        let mut changed = owner.clone();
        changed.insert(
            "registry_generation",
            json!(register_changed_registry(&store)),
        );
        assert_eq!(
            store
                .submit_classifier(&cursor, &[true, false], &changed, &Cancellation::default())
                .unwrap_err()
                .status,
            Status::StaleCursor
        );
        assert!(store.state.lock().unwrap().labels.contains_key(&cursor));
        let complete = store
            .submit_classifier(&cursor, &[true, false], &owner, &Cancellation::default())
            .unwrap();
        assert_eq!(complete["complete"].as_bool(), Some(true));
        assert!(store
            .submit_classifier(&cursor, &[true, false], &owner, &Cancellation::default())
            .is_err());
        let derived = store
            .pin(&complete["description"]["handle"], &owner)
            .unwrap();
        assert_eq!(derived.event_count, 2);
        assert!(derived.id.starts_with("labels:"));
        assert_eq!(derived.activity.turn_count(), 1);
        let page2 = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        assert_ne!(page2["cursor"].as_str(), Some(cursor.as_str()));
        let released=store.request(&json!({"schema":SCHEMA,"id":"release-labels","operation":"release","kind":"cursor","token":page2["cursor"]}),&changed,&Cancellation::default());
        assert_eq!(released["data"]["released"].as_bool(), Some(true));
        assert!(store
            .pin(&complete["description"]["handle"], &owner)
            .is_ok());
    }
    #[test]
    fn label_publication_transfers_quota_and_lease_failure_rolls_back_generation() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store=NativeStore::new(&json!({"max_leases":2,"reserved_hook_leases":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let bounds = limits(&acquire(&source.path)).unwrap();
        let classifier = json!({"id":"application","version":"1"});
        let page = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        let complete = store
            .submit_classifier(
                page["cursor"].as_str().unwrap(),
                &[true],
                &owner,
                &Cancellation::default(),
            )
            .unwrap();
        assert_eq!(complete["complete"].as_bool(), Some(true));
        assert_eq!(store.state.lock().unwrap().transient_bytes, 0);
        let reservation = store.reserve_projection(&owner, 1024 * 1024).unwrap();
        drop(reservation);
        let page = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        let generations = store.state.lock().unwrap().generations.len();
        let failed = store
            .submit_classifier(
                page["cursor"].as_str().unwrap(),
                &[true],
                &owner,
                &Cancellation::default(),
            )
            .unwrap_err();
        assert_eq!(failed.status, Status::LeaseLimit);
        let state = store.state.lock().unwrap();
        assert_eq!(state.generations.len(), generations);
        assert_eq!(state.transient_bytes, 0);
    }
    #[test]
    fn cursor_release_accepts_null_epoch_but_lease_release_does_not() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let pending = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        let response=store.request(&json!({"schema":SCHEMA,"id":"release-cursor","operation":"release","kind":"cursor","owner_epoch":null,"token":pending["cursor"]}),&owner,&Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("ok"));
        assert_eq!(response["data"]["released"].as_bool(), Some(true));
        let response=store.request(&json!({"schema":SCHEMA,"id":"release-lease","operation":"release","kind":"lease","owner_epoch":null,"token":"missing"}),&owner,&Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("invalid_request"));
    }

    #[test]
    fn lease_release_frees_evidence_when_reply_reservation_is_exhausted() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let acquired = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let handle = handle(&acquired).clone();
        let available = store.config.retained - store.retained_accounted_bytes();
        let pressure = store.reserve_projection(&owner, available).unwrap();
        let stats = store.request(
            &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(stats["status"].as_str(), Some("retained_limit"));
        let release = json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle["lease_id"]});
        let mut invalid_admission = owner.clone();
        invalid_admission.insert("admission", json!(null));
        let invalid = store.request(&release, &invalid_admission, &Cancellation::default());
        assert_eq!(invalid["status"].as_str(), Some("invalid_request"));
        let denied = store.request(&release, &context("b"), &Cancellation::default());
        assert_eq!(denied["status"].as_str(), Some("stale_handle"));
        let released = store.request(&release, &owner, &Cancellation::default());
        assert_eq!(released["status"].as_str(), Some("ok"));
        assert_eq!(released["data"]["released"].as_bool(), Some(true));
        assert_eq!(
            store.validate_scope(&handle, &owner).unwrap_err().status,
            Status::StaleHandle
        );
        drop(pressure);
        assert_eq!(store.state.lock().unwrap().transient_bytes, 0);
    }

    #[test]
    fn expiry_metadata_and_renewal_respect_original_preparation_deadline() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let handle = handle(&root).clone();
        let cap = now_ms() + 10_000;
        {
            let mut state = store.state.lock().unwrap();
            let lease = state
                .leases
                .get_mut(handle["lease_id"].as_str().unwrap())
                .unwrap();
            lease.absolute_deadline = cap;
            lease.expires = now_ms() + 1000;
        }
        let renew = store.request(
            &json!({"schema":SCHEMA,"id":"renew","operation":"renew","handle":handle}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(renew["data"]["expires_unix_ms"].as_u64(), Some(cap));
        let description = store.request(
            &json!({"schema":SCHEMA,"id":"describe-expiry","operation":"describe","handle":handle}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(
            description["data"]["description"]["lease_expires_unix_ms"].as_u64(),
            Some(cap)
        );
        let retained = store.request(
            &json!({"schema":SCHEMA,"id":"retain-capped","operation":"retain","handle":handle}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(
            retained["data"]["description"]["lease_expires_unix_ms"].as_u64(),
            Some(cap)
        );
    }
    #[test]
    fn active_borrow_extends_to_original_cap_without_delaying_revocation() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let handle = handle(&root).clone();
        let cap = now_ms() + 90_000;
        {
            let mut state = store.state.lock().unwrap();
            let lease = state
                .leases
                .get_mut(handle["lease_id"].as_str().unwrap())
                .unwrap();
            lease.absolute_deadline = cap;
            lease.expires = now_ms() + 1000;
        }
        let (_, description) = store
            .pin_scope_for_work(&handle, &owner, cap + 30_000)
            .unwrap();
        assert_eq!(description["lease_expires_unix_ms"].as_u64(), Some(cap));
        let renewed = store.request(
            &json!({"schema":SCHEMA,"id":"renew-active","operation":"renew","handle":handle}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(renewed["data"]["expires_unix_ms"].as_u64(), Some(cap));
        let (_, shorter) = store
            .pin_scope_for_work(&handle, &owner, now_ms() + 5000)
            .unwrap();
        assert_eq!(shorter["lease_expires_unix_ms"].as_u64(), Some(cap));
        let released = store.request(
            &json!({"schema":SCHEMA,"id":"revoke-active","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle["lease_id"]}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(released["data"]["released"].as_bool(), Some(true));
        assert_eq!(
            store.validate_scope(&handle, &owner).unwrap_err().status,
            Status::StaleHandle
        );
    }

    #[test]
    fn registry_registration_uses_trusted_admission_and_preserves_cached_identity() {
        let store = NativeStore::new(
            &json!({"max_retained_bytes":8*1024*1024,"reserved_hook_accounted_bytes":8*1024*1024}),
        )
        .unwrap();
        let hook = context("owner");
        let mut review = hook.clone();
        review.insert("admission", json!("review"));
        let specs = json!([{"name":"fetch","behaves_like":"Read","span_edit":null}]);
        assert_eq!(
            store
                .register_tool_registry(&specs, &review)
                .unwrap_err()
                .status,
            Status::RetainedLimit
        );
        let fingerprint = store.register_tool_registry(&specs, &hook).unwrap();
        let first = {
            let state = store.state.lock().unwrap();
            Arc::clone(&state.registries[&fingerprint].snapshot)
        };
        assert_eq!(
            store.register_tool_registry(&specs, &hook).unwrap(),
            fingerprint
        );
        let state = store.state.lock().unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &state.registries[&fingerprint].snapshot
        ));
        assert_eq!(state.transient_bytes, 0);
    }
}

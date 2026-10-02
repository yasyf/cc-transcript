use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::gateway::{sniff_provider, Provider};
use crate::snapshot_activity::ActivityIndex;
#[cfg(test)]
use crate::snapshot_ledger::Reserved;
use crate::snapshot_ledger::{
    charged_bytes, Anchor, Charge, DeadlineIndex, ExpiryIndex, LedgerEvent, LedgerHook, Ledgered,
    RetainedLedger, Table, TicketKey, Work,
};
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
    pub max_source_read_bytes: usize,
    pub max_events: usize,
    pub max_items: usize,
    pub max_output_bytes: usize,
    pub max_discovery_entries: usize,
    pub max_sources: usize,
    pub deadline_unix_ms: u64,
}

impl WorkLimits {
    pub fn to_json(&self) -> Value {
        json!({"max_read_bytes":self.max_read_bytes,"max_source_read_bytes":self.max_source_read_bytes,"max_events":self.max_events,
            "max_items":self.max_items,"max_output_bytes":self.max_output_bytes,
            "max_discovery_entries":self.max_discovery_entries,"max_sources":self.max_sources})
    }
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

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    pub device: u64,
    pub inode: u64,
    pub window_base: u64,
}

impl SourceIdentity {
    pub fn file(self) -> Self {
        Self {
            window_base: 0,
            ..self
        }
    }
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
                window_base: 0,
            },
            size: metadata.len(),
            mtime_ns: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            ctime_ns: metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
        }
    }

    pub fn windowed(mut self, tail_bytes: Option<u64>) -> Self {
        if let Some(tail_bytes) = tail_bytes.filter(|tail_bytes| self.size > *tail_bytes) {
            let quantum = (tail_bytes / 2).max(1);
            self.identity.window_base = (self.size - tail_bytes) / quantum * quantum;
        }
        self
    }

    pub fn viewed_as(mut self, pinned: SourceStamp) -> Self {
        self.identity.window_base = pinned.identity.window_base;
        self
    }

    fn revision(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}",
            self.identity.device,
            self.identity.inode,
            self.identity.window_base,
            self.size,
            self.mtime_ns,
            self.ctime_ns
        )
    }
}

#[derive(Debug)]
pub struct ChunkRows {
    ledger: LedgerHook,
    rows: Vec<Entry>,
}

impl ChunkRows {
    pub fn new(rows: Vec<Entry>) -> Self {
        Self {
            ledger: LedgerHook::default(),
            rows,
        }
    }
}

impl std::ops::Deref for ChunkRows {
    type Target = Vec<Entry>;

    fn deref(&self) -> &Vec<Entry> {
        &self.rows
    }
}

#[derive(Debug)]
pub struct EntryChunk {
    pub entries: Arc<ChunkRows>,
    pub start: usize,
    pub charge: MemoryCharge,
    pub entry_charges: Vec<MemoryCharge>,
    pub user_count: usize,
    pub sidechain_user_count: usize,
}

impl EntryChunk {
    pub fn new(start: usize, entries: Vec<Entry>) -> Self {
        let mut charge = MemoryCharge {
            owned_capacity_bytes: size_of::<Self>()
                + size_of::<ChunkRows>()
                + entries.capacity() * size_of::<Entry>(),
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
            entries: Arc::new(ChunkRows::new(entries)),
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
    pub ledger: LedgerHook,
    pub id: String,
    pub canonical_path: PathBuf,
    pub stamp: SourceStamp,
    pub provider: Provider,
    pub session_id: String,
    pub chunks: Vec<Arc<EntryChunk>>,
    pub activity: Arc<ActivityIndex>,
    pub window_start: u64,
    pub committed_bytes: u64,
    pub provisional_tail: bool,
    pub fence: Vec<u8>,
    pub event_count: usize,
    pub codex_raw: Option<Arc<Vec<u8>>>,
    pub(crate) codex_append: Option<Arc<CodexAppendIndex>>,
}

impl TranscriptSnapshot {
    pub fn from_complete_entries(
        id: String,
        canonical_path: PathBuf,
        stamp: SourceStamp,
        provider: Provider,
        session_id: String,
        entries: Vec<Entry>,
    ) -> Self {
        let event_count = entries.len();
        let activity = ActivityIndex::new(&entries.iter().collect::<Vec<_>>(), None);
        Self {
            ledger: LedgerHook::default(),
            id,
            canonical_path,
            stamp,
            provider,
            session_id,
            chunks: vec![Arc::new(EntryChunk::new(0, entries))],
            activity: Arc::new(activity),
            window_start: 0,
            committed_bytes: stamp.size,
            provisional_tail: false,
            fence: Vec::new(),
            event_count,
            codex_raw: None,
            codex_append: None,
        }
    }

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

    fn prefix_fence(&self) -> Option<(u64, &[u8])> {
        match self.provider {
            Provider::Codex => self
                .codex_raw
                .as_ref()
                .map(|raw| (self.stamp.size, &raw[raw.len().saturating_sub(64)..])),
            Provider::Claude => Some((self.committed_bytes, self.fence.as_slice())),
        }
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
        if let Some(raw) = &self.codex_raw {
            entries.push((
                Arc::as_ptr(raw) as usize,
                MemoryCharge {
                    owned_capacity_bytes: size_of::<Vec<u8>>() + raw.capacity(),
                    opaque_dom_accounted_bytes: 0,
                },
            ));
        }
        if let Some(index) = &self.codex_append {
            entries.push((
                Arc::as_ptr(index) as usize,
                MemoryCharge {
                    owned_capacity_bytes: index.accounted_bytes(),
                    opaque_dom_accounted_bytes: 0,
                },
            ));
        }
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
    prepared_fact_memory: usize,
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
    used_source_bytes: usize,
    used_events: usize,
    stage: Option<Arc<ClassifierSlot>>,
    windowed: bool,
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
    attached: AtomicBool,
    deadline: AtomicU64,
}

impl LoadSlot {
    fn ledgered_bytes(&self) -> usize {
        if self.attached.load(Ordering::Acquire) {
            self.accounted.load(Ordering::Acquire)
        } else {
            0
        }
    }
}

struct Load {
    file: File,
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    pending_start: u64,
    provider: Option<Provider>,
    chunks: Vec<Arc<EntryChunk>>,
    codex_raw: Option<Arc<Vec<u8>>>,
    codex_append: Option<Arc<CodexAppendIndex>>,
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
    window_scanned: u64,
    window_start: Option<u64>,
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

struct CachedPreparedFacts {
    stamp: SourceStamp,
    registry_generation: String,
    admission: String,
    authority: Value,
    classifier: Value,
    facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    last_used: u64,
}

enum PreparedSourceOutcome {
    Ready {
        stamp: SourceStamp,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        cached: bool,
    },
    Pending(String),
}

enum QueryPage {
    Complete(Value),
    Incomplete(Value),
}

struct PendingPreparedSource {
    token: String,
    path: PathBuf,
    stamp: SourceStamp,
}

#[derive(Clone)]
struct PreparedSourceRef {
    path: PathBuf,
    stamp: SourceStamp,
}

#[derive(Clone)]
struct WarmMembership {
    members: Arc<[PreparedSourceRef]>,
    sidechain_dirs: Arc<[(PathBuf, Option<SourceStamp>)]>,
    revision: String,
    complete: bool,
    expires: u64,
}

impl WarmMembership {
    fn accounted_bytes(&self) -> usize {
        size_of::<Self>() + self.revision.capacity()
    }

    fn anchors(&self) -> impl Iterator<Item = Anchor> {
        [
            sources_anchor(&self.members),
            sidechain_dirs_anchor(&self.sidechain_dirs),
        ]
        .into_iter()
    }
}

fn slice_key<T>(buffer: &Arc<[T]>) -> usize {
    Arc::as_ptr(buffer) as *const T as usize
}

fn sources_anchor(sources: &Arc<[PreparedSourceRef]>) -> Anchor {
    Anchor::warm(slice_key(sources), source_ref_bytes(sources))
}

fn sidechain_dirs_anchor(sidechain_dirs: &Arc<[(PathBuf, Option<SourceStamp>)]>) -> Anchor {
    Anchor::warm(
        slice_key(sidechain_dirs),
        sidechain_dir_bytes(sidechain_dirs),
    )
}

fn source_ref_bytes(sources: &[PreparedSourceRef]) -> usize {
    sources.len() * size_of::<PreparedSourceRef>()
        + sources
            .iter()
            .map(|source| source.path.as_os_str().len())
            .sum::<usize>()
}

fn sidechain_dir_bytes(sidechain_dirs: &[(PathBuf, Option<SourceStamp>)]) -> usize {
    sidechain_dirs.len() * size_of::<(PathBuf, Option<SourceStamp>)>()
        + sidechain_dirs
            .iter()
            .map(|(path, _)| path.as_os_str().len())
            .sum::<usize>()
}

struct PreparedBuild {
    claimant: String,
    context: Value,
    request: Value,
    root: Arc<TranscriptSnapshot>,
    root_handle: Value,
    classifier: Value,
    root_facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    remaining: WorkLimits,
    location_started: bool,
    location_finished: bool,
    location_cursor: Option<String>,
    located: HashMap<String, PathBuf>,
    tasks: Vec<GraphTask>,
    listing: Option<GraphListing>,
    seen: HashSet<SourceIdentity>,
    sources: Vec<PreparedSourceRef>,
    stamps: Vec<(PathBuf, SourceStamp)>,
    sidechain_dirs: Vec<(PathBuf, Option<SourceStamp>)>,
    expires: u64,
}

struct PreparedQueryCursor {
    claimant: String,
    graph_id: String,
    query: Value,
    pending: Option<PendingPreparedSource>,
    input_records: Option<VecDeque<String>>,
    next: usize,
    page_output_bytes: usize,
    remaining: WorkLimits,
    expires: u64,
}

struct PreparedGraph {
    claimant: String,
    registry_generation: String,
    admission: String,
    authority: Value,
    root: Arc<TranscriptSnapshot>,
    root_handle: Value,
    classifier: Value,
    root_facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    root_slices: HashMap<String, Arc<crate::snapshot_prepared::PreparedFacts>>,
    revision: String,
    stamps: Vec<(PathBuf, SourceStamp)>,
    validated: bool,
    sources: Arc<[PreparedSourceRef]>,
    sidechain_dirs: Arc<[(PathBuf, Option<SourceStamp>)]>,
    remaining: WorkLimits,
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
    pending_records: VecDeque<(usize, String)>,
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
    carried: HashSet<usize>,
    committed: Option<ActivityIndex>,
    result: Option<Arc<TranscriptSnapshot>>,
}

impl ClassifierStage {
    fn seeded(seed: Option<&CarriedClassification>) -> Self {
        let activity = seed.map_or_else(ActivityIndex::default, |seed| {
            seed.activity.as_ref().clone()
        });
        Self {
            indexed: activity.entry_count(),
            carried: seed.map_or_else(HashSet::new, |seed| seed.allocation_ids().collect()),
            committed: None,
            activity,
            result: None,
        }
    }

    fn accounted_bytes(&self) -> usize {
        owned_index_bytes(
            self.activity.accounted_allocations().into_iter().chain(
                self.committed
                    .iter()
                    .flat_map(ActivityIndex::accounted_allocations),
            ),
            &self.carried,
        ) + self.carried.capacity() * size_of::<usize>()
            + size_of::<Self>()
    }
}

pub(crate) fn owned_index_bytes(
    allocations: impl Iterator<Item = (usize, usize)>,
    carried: &HashSet<usize>,
) -> usize {
    let mut seen = HashSet::new();
    allocations
        .filter(|(id, _)| !carried.contains(id) && seen.insert(*id))
        .map(|(_, bytes)| bytes)
        .sum()
}

#[derive(Debug)]
pub struct CarriedClassification {
    prefix: Vec<Arc<EntryChunk>>,
    event_count: usize,
    activity: Arc<ActivityIndex>,
    touched: AtomicU64,
}

impl CarriedClassification {
    pub(crate) fn lineage(classifier_id: &str, classifier_version: &str, registry: &str) -> String {
        sonic_rs::to_string(&json!([classifier_id, classifier_version, registry]))
            .expect("classification lineage")
    }

    pub(crate) fn of(
        snapshot: &TranscriptSnapshot,
        committed: Option<ActivityIndex>,
    ) -> Option<Self> {
        let (prefix, activity) = if snapshot.provisional_tail {
            (
                &snapshot.chunks[..snapshot.chunks.len() - 1],
                Arc::new(committed?),
            )
        } else {
            (&snapshot.chunks[..], Arc::clone(&snapshot.activity))
        };
        let event_count = prefix.iter().map(|chunk| chunk.entries.len()).sum();
        assert_eq!(activity.entry_count(), event_count);
        (event_count > 0).then(|| Self {
            prefix: prefix.to_vec(),
            event_count,
            activity,
            touched: AtomicU64::new(now_ms()),
        })
    }

    pub(crate) fn extends(&self, chunks: &[Arc<EntryChunk>]) -> bool {
        self.prefix.len() <= chunks.len()
            && self
                .prefix
                .iter()
                .zip(chunks)
                .all(|(carried, chunk)| Arc::ptr_eq(carried, chunk))
    }

    pub(crate) fn activity(&self) -> &ActivityIndex {
        &self.activity
    }

    pub(crate) fn allocation_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.activity
            .accounted_allocations()
            .into_iter()
            .map(|(id, _)| id)
    }

    fn anchors(&self) -> impl Iterator<Item = Anchor> {
        self.activity
            .accounted_allocations()
            .into_iter()
            .map(Anchor::indexes)
    }
}

impl Charge<(SourceIdentity, String)> for Arc<CarriedClassification> {
    fn key_charge((_, lineage): &(SourceIdentity, String)) -> usize {
        lineage.capacity()
    }

    fn charge(&self) -> usize {
        self.prefix.capacity() * size_of::<Arc<EntryChunk>>()
    }
}

impl TicketKey for SourceIdentity {
    fn owned_bytes(&self) -> usize {
        0
    }
}

impl TicketKey for (SourceIdentity, String) {
    fn owned_bytes(&self) -> usize {
        self.1.capacity()
    }
}

pub(crate) fn committed_events(snapshot: &TranscriptSnapshot) -> usize {
    snapshot.event_count
        - if snapshot.provisional_tail {
            snapshot
                .chunks
                .last()
                .map_or(0, |chunk| chunk.entries.len())
        } else {
            0
        }
}

struct ClassifierSlot {
    work: Mutex<ClassifierStage>,
    seed: Option<Arc<CarriedClassification>>,
    accounted: AtomicUsize,
    attached: AtomicBool,
    deadline: u64,
    complete: AtomicBool,
}

impl ClassifierSlot {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.seed.iter().flat_map(|seed| seed.anchors())
    }

    fn ledgered_bytes(&self) -> usize {
        if self.attached.load(Ordering::Acquire) {
            self.accounted.load(Ordering::Acquire)
        } else {
            0
        }
    }
}

struct ClassifierProgress {
    snapshot: Option<Arc<TranscriptSnapshot>>,
    stage: Option<Arc<ClassifierSlot>>,
    read_bytes: usize,
    events: usize,
}

struct RegistryRecord {
    snapshot: Arc<crate::toolcall::ToolRegistrySnapshot>,
    allocations: Vec<(usize, usize)>,
}

impl RegistryRecord {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.allocations.iter().copied().map(Anchor::indexes)
    }
}

impl Charge<String> for RegistryRecord {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.allocations.capacity() * size_of::<(usize, usize)>()
    }
}

impl Charge<String> for LabelSlot {
    fn charge(&self) -> usize {
        self.accounted
    }
}

impl LabelSlot {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.preparation
            .seed()
            .into_iter()
            .flat_map(|seed| seed.anchors())
    }
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

    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.entries
            .iter()
            .map(|(id, charge)| Anchor::entries(*id, *charge))
            .chain(self.indexes.iter().copied().map(Anchor::indexes))
    }
}

impl Charge<usize> for GenerationRecord {
    fn charge(&self) -> usize {
        self.registry_generation.capacity()
            + self.entries.capacity() * size_of::<(usize, MemoryCharge)>()
            + self.indexes.capacity() * size_of::<(usize, usize)>()
    }
}

fn snapshot_key(snapshot: &Arc<TranscriptSnapshot>) -> usize {
    Arc::as_ptr(snapshot) as usize
}

fn facts_key(facts: &Arc<crate::snapshot_prepared::PreparedFacts>) -> usize {
    Arc::as_ptr(facts) as usize
}

fn facts_anchor(facts: &Arc<crate::snapshot_prepared::PreparedFacts>) -> Anchor {
    Anchor::facts(facts_key(facts), facts.accounted_bytes())
}

fn chunk_key(chunk: &EntryChunk) -> usize {
    Arc::as_ptr(&chunk.entries) as usize
}

fn chunk_anchor(chunk: &Arc<EntryChunk>) -> Anchor {
    Anchor::entries(chunk_key(chunk), chunk.charge)
}

fn value_bytes(value: &Value) -> usize {
    let charge = crate::snapshot_memory::value_charge(value);
    charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
}

impl Charge<String> for Lease {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.claimant.capacity() + self.registry_generation.capacity()
    }
}

impl Lease {
    fn pledge(token: &String, context: &Value) -> Result<usize, SnapshotError> {
        Ok(Self::key_charge(token)
            + str_field(context, "claimant")?.len()
            + str_field(context, "registry_generation")?.len())
    }
}

impl Charge<String> for Waiter {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.claimant.capacity() + value_bytes(&self.context)
    }
}

impl Charge<String> for ProjectionCursor {
    fn charge(&self) -> usize {
        value_bytes(&self.request)
    }
}

impl Charge<String> for DiscoveryCursor {
    fn charge(&self) -> usize {
        self.accounted
    }
}

impl Charge<String> for Checkpoint {
    fn charge(&self) -> usize {
        self.accounted
    }
}

impl Charge<String> for ResolutionCursor {
    fn charge(&self) -> usize {
        value_bytes(&self.request)
            + self.ids.iter().map(String::capacity).sum::<usize>()
            + self
                .paths
                .values()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
            + self.sessions.iter().map(value_bytes).sum::<usize>()
    }
}

impl Charge<String> for LocatedPath {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.path.as_os_str().len()
    }
}

impl Charge<String> for LocateCursor {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.accounted_bytes()
    }
}

impl Charge<String> for GraphCursor {
    fn charge(&self) -> usize {
        self.accounted
    }
}

impl Charge<String> for PreparedGraph {
    fn charge(&self) -> usize {
        size_of::<Self>()
            + self.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
            + self
                .stamps
                .iter()
                .map(|(path, _)| path.as_os_str().len())
                .sum::<usize>()
    }
}

impl Charge<String> for Arc<Mutex<PreparedGraph>> {
    fn charge(&self) -> usize {
        self.lock().expect("prepared graph").charge()
    }
}

impl PreparedGraph {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        std::iter::once(facts_anchor(&self.root_facts))
            .chain(self.root_slices.values().map(facts_anchor))
            .chain([
                sources_anchor(&self.sources),
                sidechain_dirs_anchor(&self.sidechain_dirs),
            ])
    }
}

impl Charge<String> for PreparedBuild {
    fn charge(&self) -> usize {
        size_of::<PreparedBuild>()
            + self.tasks.capacity() * size_of::<GraphTask>()
            + self.sources.capacity() * size_of::<PreparedSourceRef>()
    }
}

impl Charge<String> for PreparedQueryCursor {
    fn charge(&self) -> usize {
        size_of::<PreparedQueryCursor>()
            + self.claimant.capacity()
            + self.graph_id.capacity()
            + value_bytes(&self.query)
            + self.pending.as_ref().map_or(0, |pending| {
                pending.token.capacity() + pending.path.as_os_str().len()
            })
            + self.input_records.as_ref().map_or(0, |records| {
                records.capacity() * size_of::<String>()
                    + records.iter().map(String::capacity).sum::<usize>()
            })
    }
}

impl Charge<String> for (String, u64) {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.0.capacity()
    }
}

impl Charge<SourceIdentity> for CachedPreparedFacts {
    fn charge(&self) -> usize {
        self.registry_generation.capacity()
            + self.admission.capacity()
            + value_bytes(&self.authority)
            + value_bytes(&self.classifier)
    }
}

impl Charge<String> for WarmMembership {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.accounted_bytes()
    }
}

impl Charge<Arc<str>> for Delivery {
    fn key_charge(key: &Arc<str>) -> usize {
        2 * size_of::<usize>() + key.len()
    }

    fn charge(&self) -> usize {
        size_of::<Delivery>()
            + self.claimant.capacity()
            + self.leases.capacity() * size_of::<String>()
            + self.leases.iter().map(String::capacity).sum::<usize>()
            + self.cursor.as_ref().map_or(0, String::capacity)
    }
}

pub(crate) struct StoreState {
    latest: Table<SourceIdentity, Arc<TranscriptSnapshot>>,
    recent_codex: Table<SourceIdentity, u64>,
    loads: HashMap<SourceIdentity, Arc<LoadSlot>>,
    prepared_loads: Table<SourceIdentity, (Arc<LoadSlot>, u64)>,
    leases: Ledgered<String, Lease>,
    waiters: Ledgered<String, Waiter>,
    projections: Ledgered<String, ProjectionCursor>,
    generations: Ledgered<usize, GenerationRecord>,
    classifier_stages: HashMap<String, Arc<ClassifierSlot>>,
    carried_classifications: Ledgered<(SourceIdentity, String), Arc<CarriedClassification>>,
    graphs: Ledgered<String, GraphCursor>,
    prepared_graphs: Ledgered<String, Arc<Mutex<PreparedGraph>>>,
    prepared_builds: Ledgered<String, PreparedBuild>,
    prepared_queries: Ledgered<String, PreparedQueryCursor>,
    expired_prepared_queries: Ledgered<String, (String, u64)>,
    prepared_facts: Ledgered<SourceIdentity, CachedPreparedFacts>,
    warm_memberships: Ledgered<String, WarmMembership>,
    deliveries: Ledgered<Arc<str>, Delivery>,
    deliveries_expiry: DeadlineIndex<Arc<str>>,
    labels: Ledgered<String, LabelSlot>,
    registries: Ledgered<String, RegistryRecord>,
    discoveries: Ledgered<String, DiscoveryCursor>,
    checkpoints: Ledgered<String, Checkpoint>,
    resolutions: Ledgered<String, ResolutionCursor>,
    locations: Ledgered<String, LocatedPath>,
    locates: Ledgered<String, LocateCursor>,
    escaped_chunks: Table<usize, (Weak<ChunkRows>, MemoryCharge)>,
    recent_codex_raw_bytes: usize,
    locations_expiry: ExpiryIndex<String>,
    carried_expiry: ExpiryIndex<(SourceIdentity, String)>,
    recent_codex_expiry: ExpiryIndex<SourceIdentity>,
    prepared_loads_expiry: ExpiryIndex<SourceIdentity>,
    prepared_facts_lru: BTreeSet<(u64, SourceIdentity)>,
    counters: [u64; 18],
    transient_bytes: usize,
    owned: crate::snapshot_owned::OwnedProjections,
    ledger: RetainedLedger,
    #[cfg(test)]
    audits: Arc<AtomicUsize>,
}

const TOUCH_TTL_MS: u64 = 30 * 60_000;

fn codex_raw_len(snapshot: &Arc<TranscriptSnapshot>) -> usize {
    snapshot.codex_raw.as_ref().map_or(0, |raw| raw.len())
}

fn carried_deadline(carried: &CarriedClassification) -> u64 {
    carried
        .touched
        .load(Ordering::Acquire)
        .saturating_add(TOUCH_TTL_MS)
}

impl StoreState {
    fn new(work: Work) -> Self {
        Self {
            latest: Table::new(work.clone()),
            recent_codex: Table::new(work.clone()),
            loads: HashMap::new(),
            prepared_loads: Table::new(work.clone()),
            leases: Ledgered::new(work.clone()),
            waiters: Ledgered::new(work.clone()),
            projections: Ledgered::new(work.clone()),
            generations: Ledgered::new(work.clone()),
            classifier_stages: HashMap::new(),
            carried_classifications: Ledgered::new(work.clone()),
            graphs: Ledgered::new(work.clone()),
            prepared_graphs: Ledgered::new(work.clone()),
            prepared_builds: Ledgered::new(work.clone()),
            prepared_queries: Ledgered::new(work.clone()),
            expired_prepared_queries: Ledgered::new(work.clone()),
            prepared_facts: Ledgered::new(work.clone()),
            warm_memberships: Ledgered::new(work.clone()),
            deliveries: Ledgered::new(work.clone()),
            deliveries_expiry: DeadlineIndex::new(work.clone()),
            labels: Ledgered::new(work.clone()),
            registries: Ledgered::new(work.clone()),
            discoveries: Ledgered::new(work.clone()),
            checkpoints: Ledgered::new(work.clone()),
            resolutions: Ledgered::new(work.clone()),
            locations: Ledgered::new(work.clone()),
            locates: Ledgered::new(work.clone()),
            escaped_chunks: Table::new(work.clone()),
            recent_codex_raw_bytes: 0,
            locations_expiry: ExpiryIndex::new(work.clone()),
            carried_expiry: ExpiryIndex::new(work.clone()),
            recent_codex_expiry: ExpiryIndex::new(work.clone()),
            prepared_loads_expiry: ExpiryIndex::new(work.clone()),
            prepared_facts_lru: BTreeSet::new(),
            counters: [0; 18],
            transient_bytes: 0,
            owned: crate::snapshot_owned::OwnedProjections::default(),
            ledger: RetainedLedger::new(work),
            #[cfg(test)]
            audits: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    fn reservables(&self) -> [&dyn Reserved; 17] {
        [
            &self.ledger.shared,
            &self.registries,
            &self.generations,
            &self.carried_classifications,
            &self.leases,
            &self.waiters,
            &self.latest,
            &self.escaped_chunks,
            &self.prepared_loads,
            &self.recent_codex,
            &self.warm_memberships,
            &self.locations,
            &self.locates,
            &self.locations_expiry,
            &self.carried_expiry,
            &self.recent_codex_expiry,
            &self.prepared_loads_expiry,
        ]
    }

    fn drain(&mut self) {
        for event in self.ledger.queue.take() {
            self.ledger.shared.work().tick(1);
            self.ledger.shared.work().reclaim(1);
            match event {
                LedgerEvent::Generation(snapshot) => {
                    if let Some(record) = self.generations.get(&snapshot) {
                        assert_eq!(
                            record.snapshot.strong_count(),
                            0,
                            "generation death event for a live snapshot"
                        );
                        self.unregister_generation(snapshot);
                    }
                }
                LedgerEvent::Chunk(chunk) => {
                    if let Some((rows, _)) = self.escaped_chunks.get(&chunk) {
                        assert_eq!(rows.strong_count(), 0, "chunk death event for live rows");
                        self.release_escaped_chunk(chunk);
                    }
                }
            }
        }
    }

    fn register_generation(
        &mut self,
        snapshot: &Arc<TranscriptSnapshot>,
        record: GenerationRecord,
    ) {
        let key = snapshot_key(snapshot);
        self.generations.reserve_for(&key);
        self.ledger.shared.reserve(record.anchors());
        snapshot
            .ledger
            .arm(&self.ledger.queue, LedgerEvent::Generation(key));
        for anchor in record.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        if let Some(displaced) = self.generations.insert(key, record) {
            for anchor in displaced.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn unregister_generation(&mut self, snapshot: usize) {
        if let Some(record) = self.generations.remove(&snapshot) {
            for anchor in record.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn escaping<'a>(
        &'a self,
        chunks: &'a [Arc<EntryChunk>],
    ) -> impl Iterator<Item = &'a Arc<EntryChunk>> + 'a {
        chunks
            .iter()
            .filter(move |chunk| !self.escaped_chunks.contains_key(&chunk_key(chunk)))
    }

    fn escape_growth(&self, chunks: &[Arc<EntryChunk>]) -> usize {
        self.escaped_chunks.growth(self.escaping(chunks).count())
            + self
                .ledger
                .shared
                .admission(self.escaping(chunks).map(chunk_anchor))
    }

    fn escape_chunks(&mut self, chunks: &[Arc<EntryChunk>]) {
        let additional = self.escaping(chunks).count();
        let anchors: Vec<_> = self.escaping(chunks).map(chunk_anchor).collect();
        self.escaped_chunks.reserve(additional);
        self.ledger.shared.reserve(anchors);
        for chunk in chunks {
            let key = chunk_key(chunk);
            if self.escaped_chunks.contains_key(&key) {
                continue;
            }
            chunk
                .entries
                .ledger
                .arm(&self.ledger.queue, LedgerEvent::Chunk(key));
            self.escaped_chunks
                .insert(key, (Arc::downgrade(&chunk.entries), chunk.charge));
            self.ledger
                .shared
                .acquire(Anchor::entries(key, chunk.charge));
        }
    }

    fn release_escaped_chunk(&mut self, chunk: usize) {
        if self.escaped_chunks.remove(&chunk).is_some() {
            self.ledger.shared.release(chunk);
        }
    }

    fn insert_registry(&mut self, fingerprint: String, record: RegistryRecord) {
        self.registries.reserve_for(&fingerprint);
        self.ledger.shared.reserve(record.anchors());
        for anchor in record.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        if let Some(displaced) = self.registries.insert(fingerprint, record) {
            for anchor in displaced.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn admission<K, V: Charge<K>>(
        &self,
        key: &K,
        value: &V,
        anchors: impl IntoIterator<Item = Anchor>,
    ) -> usize {
        charged_bytes(key, value) + self.ledger.shared.admission(anchors)
    }

    fn carried_growth(&self, lineage: &(SourceIdentity, String)) -> usize {
        self.carried_classifications.growth_for(lineage)
            + self.carried_expiry.growth(1)
            + lineage.owned_bytes()
    }

    fn recent_codex_growth(&self, identity: &SourceIdentity) -> usize {
        if self.recent_codex.contains_key(identity) {
            return 0;
        }
        self.recent_codex.growth(1) + self.recent_codex_expiry.growth(1)
    }

    fn prepared_load_growth(&self, identity: &SourceIdentity) -> usize {
        self.prepared_loads.growth_for(identity) + self.prepared_loads_expiry.growth(1)
    }

    fn insert_lease(&mut self, token: String, lease: Lease) {
        self.leases.reserve_for(&token);
        self.leases.insert(token, lease);
    }

    fn insert_waiter(&mut self, token: String, waiter: Waiter) {
        self.waiters.reserve_for(&token);
        self.waiters.insert(token, waiter);
    }

    fn carries(
        &self,
        lineage: &(SourceIdentity, String),
        candidate: &CarriedClassification,
    ) -> bool {
        !self
            .carried_classifications
            .get(lineage)
            .is_some_and(|existing| {
                candidate.extends(&existing.prefix) && existing.event_count > candidate.event_count
            })
    }

    fn insert_carried(
        &mut self,
        lineage: (SourceIdentity, String),
        carried: Arc<CarriedClassification>,
    ) {
        self.carried_classifications.reserve_for(&lineage);
        self.carried_expiry.reserve(1);
        self.ledger.shared.reserve(carried.anchors());
        for anchor in carried.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        self.carried_expiry
            .push(carried_deadline(&carried), lineage.clone());
        if let Some(displaced) = self.carried_classifications.insert(lineage, carried) {
            for anchor in displaced.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
        if self
            .carried_expiry
            .crowded(self.carried_classifications.len())
        {
            self.carried_expiry.rebuild(
                self.carried_classifications.len(),
                self.carried_classifications
                    .iter()
                    .map(|(lineage, carried)| (carried_deadline(carried), lineage.clone())),
            );
        }
    }

    fn remove_carried(&mut self, lineage: &(SourceIdentity, String)) {
        if let Some(carried) = self.carried_classifications.remove(lineage) {
            for anchor in carried.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn expired_carried(&mut self, now: u64) -> Vec<(SourceIdentity, String)> {
        let carried = &self.carried_classifications;
        self.carried_expiry.expired(now, |lineage| {
            carried
                .get(lineage)
                .map(|carried| carried_deadline(carried))
        })
    }

    fn insert_latest(
        &mut self,
        identity: SourceIdentity,
        snapshot: Arc<TranscriptSnapshot>,
    ) -> Option<Arc<TranscriptSnapshot>> {
        let added = codex_raw_len(&snapshot);
        self.latest.reserve_for(&identity);
        let displaced = self.latest.insert(identity, snapshot);
        if self.recent_codex.contains_key(&identity) {
            self.recent_codex_raw_bytes -= displaced.as_ref().map_or(0, codex_raw_len);
            self.recent_codex_raw_bytes += added;
        }
        displaced
    }

    fn remove_latest(&mut self, identity: &SourceIdentity) -> Option<Arc<TranscriptSnapshot>> {
        let removed = self.latest.remove(identity)?;
        if self.recent_codex.contains_key(identity) {
            self.recent_codex_raw_bytes -= codex_raw_len(&removed);
        }
        Some(removed)
    }

    fn retain_latest(
        &mut self,
        mut keep: impl FnMut(&SourceIdentity, &Arc<TranscriptSnapshot>) -> bool,
    ) {
        let recent = &self.recent_codex;
        let raw_bytes = &mut self.recent_codex_raw_bytes;
        let work = self.ledger.shared.work();
        self.latest.retain(|identity, snapshot| {
            work.tick(1);
            let kept = keep(identity, snapshot);
            if !kept && recent.contains_key(identity) {
                *raw_bytes -= codex_raw_len(snapshot);
            }
            kept
        });
    }

    fn insert_recent_codex(&mut self, identity: SourceIdentity, now: u64) {
        if !self.recent_codex.contains_key(&identity) {
            self.recent_codex.reserve(1);
            self.recent_codex_expiry.reserve(1);
        }
        if self.recent_codex.insert(identity, now).is_some() {
            return;
        }
        self.recent_codex_raw_bytes += self.latest.get(&identity).map_or(0, codex_raw_len);
        self.recent_codex_expiry
            .push(now.saturating_add(TOUCH_TTL_MS), identity);
        if self.recent_codex_expiry.crowded(self.recent_codex.len()) {
            self.recent_codex_expiry.rebuild(
                self.recent_codex.len(),
                self.recent_codex
                    .iter()
                    .map(|(identity, touched)| (touched.saturating_add(TOUCH_TTL_MS), *identity)),
            );
        }
    }

    fn remove_recent_codex(&mut self, identity: &SourceIdentity) {
        if self.recent_codex.remove(identity).is_some() {
            self.recent_codex_raw_bytes -= self.latest.get(identity).map_or(0, codex_raw_len);
        }
    }

    fn expired_recent_codex(&mut self, now: u64) -> Vec<SourceIdentity> {
        let recent = &self.recent_codex;
        self.recent_codex_expiry.expired(now, |identity| {
            recent
                .get(identity)
                .map(|touched| touched.saturating_add(TOUCH_TTL_MS))
        })
    }

    fn insert_prepared_load(&mut self, identity: SourceIdentity, slot: Arc<LoadSlot>, now: u64) {
        self.prepared_loads.reserve_for(&identity);
        self.prepared_loads_expiry.reserve(1);
        self.prepared_loads_expiry
            .push(now.saturating_add(TOUCH_TTL_MS), identity);
        self.prepared_loads.insert(identity, (slot, now));
        if self
            .prepared_loads_expiry
            .crowded(self.prepared_loads.len())
        {
            self.prepared_loads_expiry.rebuild(
                self.prepared_loads.len(),
                self.prepared_loads.iter().map(|(identity, (_, touched))| {
                    (touched.saturating_add(TOUCH_TTL_MS), *identity)
                }),
            );
        }
    }

    fn expired_prepared_loads(&mut self, now: u64) -> Vec<SourceIdentity> {
        let loads = &self.prepared_loads;
        self.prepared_loads_expiry.expired(now, |identity| {
            loads
                .get(identity)
                .map(|(_, touched)| touched.saturating_add(TOUCH_TTL_MS))
        })
    }

    fn insert_location(&mut self, id: String, location: LocatedPath) {
        self.locations.reserve_for(&id);
        self.locations_expiry.reserve(1);
        self.locations_expiry.push(location.expires, id.clone());
        self.locations.insert(id, location);
        if self.locations_expiry.crowded(self.locations.len()) {
            self.locations_expiry.rebuild(
                self.locations.len(),
                self.locations
                    .iter()
                    .map(|(id, location)| (location.expires, id.clone())),
            );
        }
    }

    fn evict_oldest_location(&mut self) -> bool {
        let locations = &self.locations;
        let Some(oldest) = self
            .locations_expiry
            .pop_earliest(|id| locations.get(id).map(|location| location.expires))
        else {
            return false;
        };
        self.locations.remove(&oldest);
        true
    }

    fn expired_locations(&mut self, now: u64) -> Vec<String> {
        let locations = &self.locations;
        self.locations_expiry
            .expired(now, |id| locations.get(id).map(|location| location.expires))
    }

    fn retain_carried(
        &mut self,
        mut keep: impl FnMut(&(SourceIdentity, String), &Arc<CarriedClassification>) -> bool,
    ) {
        let mut released = Vec::new();
        self.carried_classifications.retain(|lineage, carried| {
            let kept = keep(lineage, carried);
            if !kept {
                released.extend(carried.anchors());
            }
            kept
        });
        for anchor in released {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn insert_label(&mut self, token: String, slot: LabelSlot) {
        self.ledger.shared.reserve(slot.anchors());
        for anchor in slot.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        if let Some(displaced) = self.labels.insert(token, slot) {
            for anchor in displaced.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn remove_label(&mut self, token: &str) -> Option<LabelSlot> {
        let slot = self.labels.remove(token)?;
        for anchor in slot.anchors() {
            self.ledger.shared.release(anchor.id);
        }
        Some(slot)
    }

    fn retain_labels(&mut self, mut keep: impl FnMut(&String, &LabelSlot) -> bool) {
        let mut released = Vec::new();
        self.labels.retain(|token, slot| {
            let kept = keep(token, slot);
            if !kept {
                released.extend(slot.anchors());
            }
            kept
        });
        for anchor in released {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn insert_classifier_stage(&mut self, key: String, slot: Arc<ClassifierSlot>) {
        self.ledger.shared.reserve(slot.anchors());
        for anchor in slot.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        slot.attached.store(true, Ordering::Release);
        self.ledger.classifier += slot.accounted.load(Ordering::Acquire);
        if let Some(displaced) = self.classifier_stages.insert(key, slot) {
            self.detach_classifier_stage(&displaced);
        }
    }

    fn detach_classifier_stage(&mut self, slot: &ClassifierSlot) {
        slot.attached.store(false, Ordering::Release);
        self.ledger.classifier = self
            .ledger
            .classifier
            .checked_sub(slot.accounted.load(Ordering::Acquire))
            .expect("balanced retained ledger");
        for anchor in slot.anchors() {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn remove_classifier_stage(&mut self, key: &str) -> Option<Arc<ClassifierSlot>> {
        let slot = self.classifier_stages.remove(key)?;
        self.detach_classifier_stage(&slot);
        Some(slot)
    }

    fn set_classifier_charge(&mut self, slot: &ClassifierSlot, bytes: usize) {
        let previous = slot.accounted.swap(bytes, Ordering::AcqRel);
        if slot.attached.load(Ordering::Acquire) {
            self.ledger.classifier = self
                .ledger
                .classifier
                .checked_sub(previous)
                .expect("balanced retained ledger")
                + bytes;
        }
    }

    fn add_classifier_charge(&mut self, slot: &ClassifierSlot, bytes: usize) {
        slot.accounted.fetch_add(bytes, Ordering::AcqRel);
        if slot.attached.load(Ordering::Acquire) {
            self.ledger.classifier += bytes;
        }
    }

    fn insert_load(&mut self, identity: SourceIdentity, slot: Arc<LoadSlot>) {
        slot.attached.store(true, Ordering::Release);
        self.ledger.pending += slot.accounted.load(Ordering::Acquire);
        if let Some(displaced) = self.loads.insert(identity, slot) {
            self.detach_load(&displaced);
        }
    }

    fn detach_load(&mut self, slot: &LoadSlot) {
        slot.attached.store(false, Ordering::Release);
        self.ledger.pending = self
            .ledger
            .pending
            .checked_sub(slot.accounted.load(Ordering::Acquire))
            .expect("balanced retained ledger");
    }

    fn remove_load(&mut self, identity: &SourceIdentity) -> Option<Arc<LoadSlot>> {
        let slot = self.loads.remove(identity)?;
        self.detach_load(&slot);
        Some(slot)
    }

    fn retain_loads(&mut self, mut keep: impl FnMut(&SourceIdentity, &Arc<LoadSlot>) -> bool) {
        let mut detached = Vec::new();
        self.loads.retain(|identity, slot| {
            let kept = keep(identity, slot);
            if !kept {
                detached.push(Arc::clone(slot));
            }
            kept
        });
        for slot in detached {
            self.detach_load(&slot);
        }
    }

    fn set_load_charge(&mut self, slot: &LoadSlot, bytes: usize) {
        let previous = slot.accounted.swap(bytes, Ordering::AcqRel);
        if slot.attached.load(Ordering::Acquire) {
            self.ledger.pending = self
                .ledger
                .pending
                .checked_sub(previous)
                .expect("balanced retained ledger")
                + bytes;
        }
    }

    fn add_load_charge(&mut self, slot: &LoadSlot, bytes: usize) {
        slot.accounted.fetch_add(bytes, Ordering::AcqRel);
        if slot.attached.load(Ordering::Acquire) {
            self.ledger.pending += bytes;
        }
    }

    fn prepared_facts_growth(&self, identity: &SourceIdentity) -> usize {
        usize::from(!self.prepared_facts.contains_key(identity))
            * size_of::<(u64, SourceIdentity)>()
    }

    fn insert_prepared_facts(&mut self, identity: SourceIdentity, cached: CachedPreparedFacts) {
        self.ledger.shared.reserve([facts_anchor(&cached.facts)]);
        self.ledger.shared.acquire(facts_anchor(&cached.facts));
        let last_used = cached.last_used;
        if let Some(displaced) = self.prepared_facts.insert(identity, cached) {
            self.ledger.shared.release(facts_key(&displaced.facts));
            self.prepared_facts_lru
                .remove(&(displaced.last_used, identity));
        }
        self.prepared_facts_lru.insert((last_used, identity));
    }

    fn remove_prepared_facts(&mut self, identity: &SourceIdentity) -> Option<CachedPreparedFacts> {
        let cached = self.prepared_facts.remove(identity)?;
        self.ledger.shared.release(facts_key(&cached.facts));
        self.prepared_facts_lru
            .remove(&(cached.last_used, *identity));
        Some(cached)
    }

    fn evictable_prepared_facts(&self) -> Option<SourceIdentity> {
        let work = self.ledger.shared.work();
        self.prepared_facts_lru
            .iter()
            .map(|(_, identity)| *identity)
            .inspect(|_| work.tick(1))
            .find(|identity| Arc::strong_count(&self.prepared_facts[identity].facts) == 1)
    }

    fn touch_prepared_facts(
        &mut self,
        stamp: SourceStamp,
        registry_generation: &str,
        admission: &str,
        authority: &Value,
        classifier: &Value,
    ) -> Option<Arc<crate::snapshot_prepared::PreparedFacts>> {
        let mut cached = self.prepared_facts.get_mut(&stamp.identity)?;
        if cached.stamp != stamp
            || cached.registry_generation != registry_generation
            || cached.admission != admission
            || cached.authority != *authority
            || cached.classifier != *classifier
        {
            return None;
        }
        let previous = cached.last_used;
        cached.last_used = now_ms();
        self.prepared_facts_lru.remove(&(previous, stamp.identity));
        self.prepared_facts_lru
            .insert((cached.last_used, stamp.identity));
        Some(Arc::clone(&cached.facts))
    }

    fn insert_prepared_graph(&mut self, graph_id: String, graph: Arc<Mutex<PreparedGraph>>) {
        let prepared = graph.lock().expect("prepared graph");
        self.ledger.shared.reserve(prepared.anchors());
        for anchor in prepared.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        drop(prepared);
        if let Some(displaced) = self.prepared_graphs.insert(graph_id, graph) {
            self.release_prepared_graph(&displaced);
        }
    }

    fn release_prepared_graph(&mut self, graph: &Mutex<PreparedGraph>) {
        for anchor in graph.lock().expect("prepared graph").anchors() {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn remove_prepared_graph(&mut self, graph_id: &str) -> Option<Arc<Mutex<PreparedGraph>>> {
        let graph = self.prepared_graphs.remove(graph_id)?;
        self.release_prepared_graph(&graph);
        Some(graph)
    }

    fn retain_prepared_graphs(
        &mut self,
        mut keep: impl FnMut(&String, &Arc<Mutex<PreparedGraph>>) -> bool,
    ) {
        let mut released = Vec::new();
        self.prepared_graphs.retain(|graph_id, graph| {
            let kept = keep(graph_id, graph);
            if !kept {
                released.push(Arc::clone(graph));
            }
            kept
        });
        for graph in released {
            self.release_prepared_graph(&graph);
        }
    }

    fn insert_root_slice(
        &mut self,
        graph: &mut PreparedGraph,
        key: String,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    ) {
        self.ledger.shared.reserve([facts_anchor(&facts)]);
        self.ledger.shared.acquire(facts_anchor(&facts));
        graph.root_slices.insert(key, facts);
    }

    fn insert_delivery(&mut self, key: Arc<str>, delivery: Delivery) {
        self.remove_delivery(&key);
        self.deliveries_expiry
            .insert(delivery.expires, Arc::clone(&key));
        self.deliveries.insert(key, delivery);
    }

    fn remove_delivery(&mut self, key: &str) -> Option<Delivery> {
        let (key, delivery) = self.deliveries.remove_entry(key)?;
        self.deliveries_expiry.remove(delivery.expires, key);
        Some(delivery)
    }

    fn expire_deliveries(&mut self, now: u64) {
        while let Some(key) = self.deliveries_expiry.pop_expired(now) {
            self.deliveries.remove(&*key).expect("indexed delivery");
        }
    }

    fn evict_earliest_delivery(&mut self) {
        if let Some(earliest) = self.deliveries_expiry.earliest().cloned() {
            self.remove_delivery(&earliest);
        }
    }

    fn insert_warm_membership(&mut self, key: String, membership: WarmMembership) {
        self.warm_memberships.reserve_for(&key);
        self.ledger.shared.reserve(membership.anchors());
        for anchor in membership.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        if let Some(displaced) = self.warm_memberships.insert(key, membership) {
            self.release_warm_membership(&displaced);
        }
    }

    fn release_warm_membership(&mut self, membership: &WarmMembership) {
        for anchor in membership.anchors() {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn remove_warm_membership(&mut self, key: &str) -> Option<WarmMembership> {
        let membership = self.warm_memberships.remove(key)?;
        self.release_warm_membership(&membership);
        Some(membership)
    }

    fn retain_warm_memberships(&mut self, mut keep: impl FnMut(&String, &WarmMembership) -> bool) {
        let mut released = Vec::new();
        self.warm_memberships.retain(|key, membership| {
            let kept = keep(key, membership);
            if !kept {
                released.extend(membership.anchors());
            }
            kept
        });
        for anchor in released {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn insert_prepared_build(&mut self, token: String, build: PreparedBuild) {
        self.ledger
            .shared
            .reserve([facts_anchor(&build.root_facts)]);
        self.ledger.shared.acquire(facts_anchor(&build.root_facts));
        if let Some(displaced) = self.prepared_builds.insert(token, build) {
            self.ledger.shared.release(facts_key(&displaced.root_facts));
        }
    }

    fn remove_prepared_build(&mut self, token: &str) -> Option<PreparedBuild> {
        let build = self.prepared_builds.remove(token)?;
        self.ledger.shared.release(facts_key(&build.root_facts));
        Some(build)
    }

    fn retain_classifier_stages(
        &mut self,
        mut keep: impl FnMut(&String, &Arc<ClassifierSlot>) -> bool,
    ) {
        let mut detached = Vec::new();
        self.classifier_stages.retain(|key, slot| {
            let kept = keep(key, slot);
            if !kept {
                detached.push(Arc::clone(slot));
            }
            kept
        });
        for slot in detached {
            self.detach_classifier_stage(&slot);
        }
    }
}

struct GaugeTerms {
    entries: usize,
    indexes: usize,
    pending: usize,
    classifier: usize,
    label: usize,
    discovery: usize,
    projections: usize,
    graph: usize,
    metadata: usize,
    live_generations: usize,
}

impl GaugeTerms {
    fn report(&self, state: &StoreState) -> Value {
        let metadata = self.metadata + NativeStore::fixed_metadata_bytes(state);
        let owned = state.owned.accounted_bytes();
        json!({"retained_entry_capacity_bytes": self.entries, "retained_index_capacity_bytes": self.indexes,
            "retained_projection_bytes": self.projections + self.discovery + self.graph + metadata + owned, "pending_input_capacity_bytes": self.pending + self.classifier + self.label + state.transient_bytes,
            "retained_total_accounted_bytes": self.entries + self.indexes + self.pending + self.classifier + self.label + self.projections + self.discovery + self.graph + metadata + state.transient_bytes + owned,
            "active_leases": state.leases.len(), "pending_loads": state.loads.len(), "live_generations": self.live_generations})
    }
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
    prepared_disk: crate::snapshot_prepared_disk::PreparedDiskCache,
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
    #[cfg(test)]
    membership_metadata_checks: AtomicUsize,
    #[cfg(test)]
    pub(crate) retained_work: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) reclaims: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) warm_copies: AtomicUsize,
    #[cfg(test)]
    pub(crate) audits: Arc<AtomicUsize>,
}

pub(crate) struct ProjectionReservation<'a> {
    store: &'a NativeStore,
    bytes: usize,
}

impl Drop for ProjectionReservation<'_> {
    fn drop(&mut self) {
        self.store.lock_state().transient_bytes -= self.bytes;
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
        let mut state = self.store.lock_state();
        if self.keep {
            if let Some(mut waiter) = state.waiters.get_mut(self.token) {
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

pub(crate) fn source_read_limit() -> SnapshotError {
    SnapshotError::new(Status::Incomplete, "source_read_limit")
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

fn tail_bytes(request: &Value) -> Result<Option<u64>, SnapshotError> {
    request
        .get("tail_bytes")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|tail_bytes| *tail_bytes > 0)
                .ok_or_else(|| invalid("tail_bytes must be a positive integer"))
        })
        .transpose()
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
        max_source_read_bytes: number(value, "max_source_read_bytes")?,
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
            prepared_fact_memory: get("max_prepared_fact_memory_bytes", 128 * 1024 * 1024),
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
            || config.prepared_fact_memory == 0
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
        let work = Work::default();
        let mut state = StoreState::new(work.clone());
        for registry in [captured, builtin] {
            let fingerprint = registry.fingerprint().to_owned();
            if !state.registries.contains_key(&fingerprint) {
                state.insert_registry(
                    fingerprint,
                    RegistryRecord {
                        allocations: registry.accounted_allocations(),
                        snapshot: registry,
                    },
                );
            }
        }
        #[cfg(test)]
        let audits = Arc::clone(&state.audits);
        Ok(Self {
            config,
            prepared_disk: crate::snapshot_prepared_disk::PreparedDiskCache::new(
                &owner_epoch,
                get("max_prepared_disk_bytes", 2 * 1024 * 1024 * 1024),
            )?,
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
            #[cfg(test)]
            membership_metadata_checks: AtomicUsize::new(0),
            #[cfg(test)]
            retained_work: work.counter(),
            #[cfg(test)]
            reclaims: work.reclaims(),
            #[cfg(test)]
            warm_copies: AtomicUsize::new(0),
            #[cfg(test)]
            audits,
        })
    }

    pub(crate) fn lock_state(&self) -> MutexGuard<'_, StoreState> {
        let mut state = self.state.lock().expect("snapshot state");
        state.drain();
        state
    }

    pub fn scan_limits(&self) -> WorkLimits {
        WorkLimits {
            max_read_bytes: self.config.read_step,
            max_source_read_bytes: self.config.read_step,
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
        let definitions =
            crate::toolcall::ToolRegistrySnapshot::specs_from_json(specs).map_err(invalid)?;
        let fingerprint = crate::toolcall::ToolRegistrySnapshot::fingerprint_of(&definitions);
        if self
            .state
            .lock()
            .expect("snapshot state")
            .registries
            .contains_key(&fingerprint)
        {
            return Ok(fingerprint);
        }
        let charge = crate::snapshot_memory::value_charge(specs);
        let reserve = (charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
            .saturating_mul(4)
            .saturating_add(64 * 1024);
        let mut reservation = self.reserve_projection(context, reserve)?;
        let registry = crate::toolcall::ToolRegistrySnapshot::from_specs(definitions);
        let allocations = registry.accounted_allocations();
        let bytes = allocations.iter().map(|(_, bytes)| *bytes).sum::<usize>()
            + allocations.capacity() * size_of::<(usize, usize)>()
            + size_of::<RegistryRecord>()
            + fingerprint.capacity();
        let mut state = self.lock_state();
        if !state.registries.contains_key(&fingerprint) {
            let record = RegistryRecord {
                snapshot: registry,
                allocations,
            };
            let retained =
                bytes + state.registries.growth(1) + state.ledger.shared.growth(record.anchors());
            if retained > reservation.bytes {
                return Err(SnapshotError::new(
                    Status::RetainedLimit,
                    "tool registry allocation exceeds reservation",
                ));
            }
            state.insert_registry(fingerprint.clone(), record);
            state.transient_bytes -= retained;
            reservation.bytes -= retained;
        }
        Ok(fingerprint)
    }

    pub(crate) fn registry(
        &self,
        context: &Value,
    ) -> Result<Arc<crate::toolcall::ToolRegistrySnapshot>, SnapshotError> {
        self.lock_state()
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
        let state = self.lock_state();
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
        let state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
        Self::prune(&mut state);
        let additional = required - reservation.bytes;
        self.admit_memory(&mut state, context, additional)?;
        let cap = self.memory_cap(context)?;
        let used = number(&Self::gauges(&mut state), "retained_total_accounted_bytes")?;
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let retained = slot.accounted + state.ledger.shared.growth(slot.anchors());
        if retained > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "classifier retained bytes exceed reservation",
            ));
        }
        state.transient_bytes -= retained;
        reservation.bytes -= retained;
        state.insert_label(token, slot);
        Ok(())
    }

    fn publish_label_generation(
        &self,
        snapshot: Arc<TranscriptSnapshot>,
        generation: GenerationRecord,
        classifier: Value,
        seed: Option<&CarriedClassification>,
        carried: Option<((SourceIdentity, String), CarriedClassification)>,
        source_handle: &Value,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Value, SnapshotError> {
        let shared: HashSet<_> = snapshot
            .chunks
            .iter()
            .map(|chunk| Arc::as_ptr(&chunk.entries) as usize)
            .collect();
        let seeded: HashSet<_> = seed
            .into_iter()
            .flat_map(CarriedClassification::allocation_ids)
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
                .filter(|(id, _)| !seeded.contains(id))
                .map(|(_, bytes)| *bytes)
                .sum::<usize>()
            + generation.entries.capacity() * size_of::<(usize, MemoryCharge)>()
            + generation.indexes.capacity() * size_of::<(usize, usize)>()
            + size_of::<GenerationRecord>()
            + snapshot.id.capacity();
        let token = self.token("lease");
        let pledge = Lease::pledge(&token, context)?;
        let mut state = self.lock_state();
        let absolute_deadline = self
            .lease(&state, source_handle, context)?
            .absolute_deadline;
        let key = snapshot_key(&snapshot);
        let carried = carried
            .filter(|(lineage, candidate)| state.carries(lineage, candidate))
            .map(|(lineage, candidate)| (lineage, Arc::new(candidate)));
        let published: HashSet<_> = generation.anchors().map(|anchor| anchor.id).collect();
        let anchors = || {
            generation.anchors().chain(
                carried
                    .iter()
                    .flat_map(|(_, candidate)| candidate.anchors()),
            )
        };
        let pledge = pledge + state.leases.growth(1);
        let retained = retained
            + pledge
            + state.generations.growth_for(&key)
            + state.ledger.shared.growth(anchors())
            + carried.as_ref().map_or(0, |(lineage, candidate)| {
                charged_bytes(lineage, candidate)
                    + state.carried_growth(lineage)
                    + state.ledger.shared.unowned_bytes(
                        candidate
                            .anchors()
                            .filter(|anchor| !published.contains(&anchor.id)),
                    )
            });
        if retained > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "derived classifier publication exceeds reservation",
            ));
        }
        state.ledger.shared.reserve(anchors());
        state.register_generation(&snapshot, generation);
        let data = match self.issue(
            &mut state,
            snapshot,
            classifier,
            context,
            absolute_deadline,
            token,
            pledge,
        ) {
            Ok(data) => data,
            Err(error) => {
                state.unregister_generation(key);
                return Err(error);
            }
        };
        if let Some((lineage, carried)) = carried {
            state.insert_carried(lineage, carried);
        }
        state.transient_bytes -= retained - pledge;
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
            self.lock_state().counters[if error.status == Status::Cancelled {
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
        let lineage = (
            source.stamp.identity,
            CarriedClassification::lineage(
                &binding.classifier_id,
                &binding.classifier_version,
                &binding.registry_generation,
            ),
        );
        let mut preparation = {
            let mut state = self.lock_state();
            crate::snapshot_labels::LabelPreparation::new(
                Arc::clone(&source),
                binding.clone(),
                bounds,
                max_stage_bytes,
                Self::carried(&mut state, &lineage, &source.chunks),
            )?
        };
        if preparation.complete() {
            let work = preparation.usage();
            let classifier = preparation.derived_classifier();
            let seed = preparation.seed().cloned();
            let (snapshot, carried) = preparation.finish(&binding, cancel)?;
            let generation = GenerationRecord::new(&snapshot, &binding.registry_generation);
            let data = self.publish_label_generation(
                snapshot,
                generation,
                classifier,
                seed.as_deref(),
                carried.map(|carried| (lineage, carried)),
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
            self.lock_state().counters[if error.status == Status::Cancelled {
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
            let mut state = self.lock_state();
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
            let mut state = self.lock_state();
            self.lease(&state, &source_handle, context)?;
            let slot = state.remove_label(cursor).ok_or_else(|| {
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
            let lineage = (
                slot.preparation.source().stamp.identity,
                CarriedClassification::lineage(
                    &binding.classifier_id,
                    &binding.classifier_version,
                    &binding.registry_generation,
                ),
            );
            let seed = slot.preparation.seed().cloned();
            let (snapshot, carried) = slot.preparation.finish(&binding, cancel)?;
            let generation = GenerationRecord::new(&snapshot, &binding.registry_generation);
            let data = self.publish_label_generation(
                snapshot,
                generation,
                classifier,
                seed.as_deref(),
                carried.map(|carried| (lineage, carried)),
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
        self.lock_state().counters[14] += bytes as u64;
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
                .lock_state()
                .generations
                .get(&snapshot_key(&snapshot))
                .is_some_and(|generation| generation.registry_generation == registry)
        {
            return Ok(ClassifierProgress {
                snapshot: Some(snapshot),
                stage: None,
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
        let key = sonic_rs::to_string(&json!([snapshot.id, classifier_key, registry]))
            .expect("classified key");
        let lineage = (
            snapshot.stamp.identity,
            CarriedClassification::lineage(id, version, registry),
        );
        let committed = committed_events(&snapshot);
        let slot = {
            let mut state = self.lock_state();
            Self::prune(&mut state);
            if let Some(derived) = self
                .classified
                .lock()
                .expect("classified snapshots")
                .get(&key)
                .cloned()
            {
                return Ok(ClassifierProgress {
                    snapshot: Some(derived),
                    stage: None,
                    read_bytes: 0,
                    events: 0,
                });
            }
            if let Some(slot) = state.classifier_stages.get(&key) {
                Arc::clone(slot)
            } else {
                let cap = if str_field(context, "admission")? == "hook" {
                    self.config.loads
                } else {
                    self.config.loads.saturating_sub(self.config.hook_loads)
                };
                if state
                    .classifier_stages
                    .values()
                    .filter(|slot| Arc::strong_count(slot) > 1)
                    .count()
                    >= cap
                {
                    return Err(SnapshotError::new(
                        Status::RetainedLimit,
                        "classifier preparation admission exhausted",
                    ));
                }
                while state.classifier_stages.len() >= cap {
                    let idle = state
                        .classifier_stages
                        .iter()
                        .filter(|(_, slot)| Arc::strong_count(slot) == 1)
                        .min_by_key(|(_, slot)| slot.deadline)
                        .map(|(key, _)| key.clone())
                        .expect("idle classifier stage below held cap");
                    state.remove_classifier_stage(&idle);
                }
                let seed = Self::carried(&mut state, &lineage, &snapshot.chunks);
                let mut stage = ClassifierStage::seeded(seed.as_deref());
                if snapshot.provisional_tail && stage.indexed == committed {
                    stage.committed = Some(stage.activity.clone());
                }
                let accounted = stage.accounted_bytes();
                let additional = size_of::<ClassifierSlot>()
                    + accounted
                    + state
                        .ledger
                        .shared
                        .growth(seed.iter().flat_map(|seed| seed.anchors()));
                self.admit_memory(&mut state, context, additional)?;
                let slot = Arc::new(ClassifierSlot {
                    work: Mutex::new(stage),
                    seed,
                    accounted: AtomicUsize::new(accounted),
                    attached: AtomicBool::new(false),
                    deadline: now_ms() + self.config.preparation,
                    complete: AtomicBool::new(false),
                });
                state.insert_classifier_stage(key.clone(), Arc::clone(&slot));
                slot
            }
        };
        cancel.check(slot.deadline.min(bounds.deadline_unix_ms))?;
        let Ok(mut stage) = slot.work.try_lock() else {
            return Ok(ClassifierProgress {
                snapshot: None,
                stage: Some(Arc::clone(&slot)),
                read_bytes: 0,
                events: 0,
            });
        };
        if let Some(result) = &stage.result {
            return Ok(ClassifierProgress {
                snapshot: Some(Arc::clone(result)),
                stage: None,
                read_bytes: 0,
                events: 0,
            });
        }
        let start = stage.indexed;
        let limit = if start < committed {
            committed
        } else {
            snapshot.event_count
        }
        .min(start + self.config.event_step.min(bounds.max_events));
        if start < snapshot.event_count && limit == start {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "classifier event budget exhausted",
            ));
        }
        let mut bytes = 0usize;
        let mut stop = start;
        while stop < limit {
            let at = snapshot.chunks.partition_point(|chunk| chunk.start <= stop) - 1;
            let chunk = &snapshot.chunks[at];
            let charge = chunk.entry_charges[stop - chunk.start];
            let charged = bytes
                .saturating_add(charge.owned_capacity_bytes)
                .saturating_add(charge.opaque_dom_accounted_bytes);
            if charged > bounds.max_read_bytes {
                break;
            }
            bytes = charged;
            stop += 1;
        }
        if stop == start && start < limit {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "classifier read budget exhausted before callback",
            ));
        }
        let reserve = bytes.saturating_mul(2).saturating_add(stop - start);
        {
            let mut state = self.lock_state();
            self.admit_memory(&mut state, context, reserve)?;
            state.add_classifier_charge(&slot, reserve);
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
            if snapshot.provisional_tail && stop == committed {
                stage.committed = Some(stage.activity.clone());
            }
            usage[6] += 1;
        }
        self.lock_state()
            .set_classifier_charge(&slot, stage.accounted_bytes());
        cancel.check(slot.deadline.min(bounds.deadline_unix_ms))?;
        if stop < snapshot.event_count {
            return Ok(ClassifierProgress {
                snapshot: None,
                stage: Some(Arc::clone(&slot)),
                read_bytes: bytes,
                events: stop - start,
            });
        }
        let derived = Arc::new(TranscriptSnapshot {
            ledger: LedgerHook::default(),
            id: self.token("classified"),
            canonical_path: snapshot.canonical_path.clone(),
            stamp: snapshot.stamp,
            provider: snapshot.provider,
            session_id: snapshot.session_id.clone(),
            chunks: snapshot.chunks.clone(),
            activity: Arc::new(stage.activity.clone()),
            window_start: snapshot.window_start,
            committed_bytes: snapshot.committed_bytes,
            provisional_tail: snapshot.provisional_tail,
            fence: snapshot.fence.clone(),
            event_count: snapshot.event_count,
            codex_raw: snapshot.codex_raw.clone(),
            codex_append: snapshot.codex_append.clone(),
        });
        let generation = GenerationRecord::new(&derived, registry);
        let carried = CarriedClassification::of(&derived, stage.committed.clone()).map(Arc::new);
        {
            let mut state = self.lock_state();
            let carried = carried.filter(|candidate| state.carries(&lineage, candidate));
            let key = snapshot_key(&derived);
            let anchors = || {
                generation
                    .anchors()
                    .chain(carried.iter().flat_map(|candidate| candidate.anchors()))
            };
            let additional = state.admission(&key, &generation, anchors())
                + state.generations.growth_for(&key)
                + carried.as_ref().map_or(0, |candidate| {
                    charged_bytes(&lineage, candidate) + state.carried_growth(&lineage)
                });
            self.admit_memory(
                &mut state,
                context,
                additional.saturating_sub(slot.ledgered_bytes()),
            )?;
            state.ledger.shared.reserve(anchors());
            state.set_classifier_charge(&slot, 0);
            state.register_generation(&derived, generation);
            if let Some(candidate) = carried {
                state.insert_carried(lineage, candidate);
            }
        }
        stage.activity = ActivityIndex::default();
        stage.committed = None;
        stage.result = Some(Arc::clone(&derived));
        self.classified
            .lock()
            .expect("classified snapshots")
            .insert(key, Arc::clone(&derived));
        slot.complete.store(true, Ordering::Release);
        Ok(ClassifierProgress {
            snapshot: Some(derived),
            stage: None,
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

    fn carried(
        state: &mut StoreState,
        lineage: &(SourceIdentity, String),
        chunks: &[Arc<EntryChunk>],
    ) -> Option<Arc<CarriedClassification>> {
        let carried = state
            .carried_classifications
            .get(lineage)
            .filter(|carried| carried.extends(chunks))?;
        carried.touched.store(now_ms(), Ordering::Release);
        Some(Arc::clone(carried))
    }

    fn prune(state: &mut StoreState) {
        Self::prune_at(state, now_ms());
    }

    fn prune_at(state: &mut StoreState, now: u64) {
        state.drain();
        for lineage in state.expired_carried(now) {
            state.remove_carried(&lineage);
        }
        state
            .expired_prepared_queries
            .retain(|_, (_, expires)| *expires > now);
        for identity in state.expired_recent_codex(now) {
            state.remove_recent_codex(&identity);
            state.remove_latest(&identity);
        }
        state.expire_deliveries(now);
        state.owned.prune(now);
        state
            .retain_prepared_graphs(|_, graph| graph.lock().expect("prepared graph").expires > now);
        let expired_queries: Vec<_> = state
            .prepared_queries
            .iter()
            .filter(|(_, cursor)| cursor.expires <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired_queries {
            if let Some(query) = state.prepared_queries.remove(&token) {
                if let Some(pending) = query.pending {
                    state.waiters.remove(&pending.token);
                }
                if query.remaining.deadline_unix_ms <= now {
                    if state.expired_prepared_queries.len() >= 1024 {
                        let oldest = state
                            .expired_prepared_queries
                            .iter()
                            .min_by_key(|(_, (_, expires))| expires)
                            .map(|(token, _)| token.clone())
                            .expect("full expired prepared query cache");
                        state.expired_prepared_queries.remove(&oldest);
                    }
                    state
                        .expired_prepared_queries
                        .insert(token, (query.claimant, now.saturating_add(5_000)));
                }
            }
        }
        let expired_builds: Vec<_> = state
            .prepared_builds
            .iter()
            .filter(|(_, cursor)| cursor.expires <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired_builds {
            if let Some(build) = state.remove_prepared_build(&token) {
                Self::release_prepared_build_state(state, &build);
            }
        }
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
        state.retain_classifier_stages(|_, slot| {
            slot.deadline > now && !slot.complete.load(Ordering::Acquire)
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
        for id in state.expired_locations(now) {
            state.locations.remove(&id);
        }
        state
            .locates
            .retain(|_, cursor| cursor.expires > now && cursor.limits.deadline_unix_ms > now);
        state.leases.retain(|_, lease| lease.expires > now);
        state
            .waiters
            .retain(|_, waiter| waiter.expires > now && waiter.deadline > now);
        state
            .projections
            .retain(|_, cursor| cursor.expires > now && cursor.limits.deadline_unix_ms > now);
        let active: HashSet<_> = state
            .waiters
            .values()
            .map(|w| w.load.stamp.identity)
            .collect();
        for identity in state.expired_prepared_loads(now) {
            state.prepared_loads.remove(&identity);
        }
        state.retain_warm_memberships(|_, membership| membership.expires > now);
        state.retain_loads(|id, slot| {
            (active.contains(id) && slot.deadline.load(Ordering::Acquire) > now)
                || Arc::strong_count(slot) > 1
        });
    }

    #[cfg(test)]
    const GAUGE_KEYS: [&'static str; 8] = [
        "retained_entry_capacity_bytes",
        "retained_index_capacity_bytes",
        "retained_projection_bytes",
        "pending_input_capacity_bytes",
        "retained_total_accounted_bytes",
        "active_leases",
        "pending_loads",
        "live_generations",
    ];

    fn gauges(state: &mut StoreState) -> Value {
        state.drain();
        GaugeTerms {
            entries: state.ledger.shared.entries(),
            indexes: state.ledger.shared.indexes(),
            pending: state.ledger.pending,
            classifier: state.ledger.classifier,
            label: state.labels.charged(),
            discovery: state.discoveries.charged() + state.checkpoints.charged(),
            projections: state.projections.charged(),
            graph: state.graphs.charged()
                + state.prepared_graphs.charged()
                + state.prepared_queries.charged()
                + state.prepared_builds.charged()
                + state.expired_prepared_queries.charged()
                + state.prepared_facts.charged()
                + state.ledger.shared.facts(),
            metadata: state.deliveries.charged()
                + state.registries.charged()
                + state.generations.charged()
                + state.carried_classifications.charged()
                + state.leases.charged()
                + state.waiters.charged()
                + state.resolutions.charged()
                + state.warm_memberships.charged()
                + state.ledger.shared.warm()
                + state.locations.charged()
                + state.locates.charged(),
            live_generations: state.generations.len(),
        }
        .report(state)
    }

    #[cfg(test)]
    fn audit_gauges(state: &StoreState) -> Value {
        state.audits.fetch_add(1, Ordering::Relaxed);
        let mut allocations = HashSet::new();
        let mut snapshots = HashSet::new();
        let mut entries = 0usize;
        let mut indexes = 0usize;
        for record in state.generations.values() {
            let Some(snapshot) = record.snapshot.upgrade() else {
                continue;
            };
            snapshots.insert(snapshot_key(&snapshot));
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
        let seeds = state
            .carried_classifications
            .values()
            .chain(
                state
                    .classifier_stages
                    .values()
                    .filter_map(|slot| slot.seed.as_ref()),
            )
            .chain(
                state
                    .labels
                    .values()
                    .filter_map(|slot| slot.preparation.seed()),
            );
        for carried in seeds {
            for (id, bytes) in carried.activity.accounted_allocations() {
                if allocations.insert(id) {
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
        GaugeTerms {
            entries,
            indexes,
            pending: state
                .loads
                .values()
                .map(|slot| slot.accounted.load(Ordering::Acquire))
                .sum(),
            classifier: state
                .classifier_stages
                .values()
                .map(|slot| slot.accounted.load(Ordering::Acquire))
                .sum(),
            label: state.labels.audit_charged(),
            discovery: state.discoveries.audit_charged() + state.checkpoints.audit_charged(),
            projections: state.projections.audit_charged(),
            graph: state.graphs.audit_charged()
                + state.prepared_graphs.audit_charged()
                + state.prepared_queries.audit_charged()
                + state.prepared_builds.audit_charged()
                + state.expired_prepared_queries.audit_charged()
                + state.prepared_facts.audit_charged()
                + Self::audit_prepared_fact_bytes(state),
            metadata: state.deliveries.audit_charged()
                + state.registries.audit_charged()
                + state.generations.audit_charged()
                + state.carried_classifications.audit_charged()
                + state.leases.audit_charged()
                + state.waiters.audit_charged()
                + state.resolutions.audit_charged()
                + state.warm_memberships.audit_charged()
                + Self::audit_warm_buffer_bytes(state)
                + state.locations.audit_charged()
                + state.locates.audit_charged(),
            live_generations: snapshots.len(),
        }
        .report(state)
    }

    fn fixed_metadata_bytes(state: &StoreState) -> usize {
        Self::bookkeeping_bytes(state) + state.ledger.queue.buffer_bytes()
    }

    fn bookkeeping_bytes(state: &StoreState) -> usize {
        size_of::<StoreState>()
            + state.ledger.shared.table_bytes()
            + state.registries.capacity_bytes()
            + state.generations.capacity_bytes()
            + state.carried_classifications.capacity_bytes()
            + state.leases.capacity_bytes()
            + state.waiters.capacity_bytes()
            + state.latest.reserved_bytes()
            + state.escaped_chunks.reserved_bytes()
            + state.prepared_loads.reserved_bytes()
            + state.recent_codex.reserved_bytes()
            + state.warm_memberships.capacity_bytes()
            + state.locations.capacity_bytes()
            + state.locates.capacity_bytes()
            + state.locations_expiry.heap_bytes()
            + state.carried_expiry.heap_bytes()
            + state.recent_codex_expiry.heap_bytes()
            + state.prepared_loads_expiry.heap_bytes()
            + state.deliveries_expiry.index_bytes()
            + state.prepared_facts_lru.len() * size_of::<(u64, SourceIdentity)>()
    }

    #[cfg(test)]
    fn audit_recent_codex_raw_bytes(state: &StoreState) -> usize {
        state
            .recent_codex
            .keys()
            .filter_map(|identity| state.latest.get(identity))
            .map(codex_raw_len)
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn retained_accounted_bytes(&self) -> usize {
        let mut state = self.lock_state();
        number(&Self::gauges(&mut state), "retained_total_accounted_bytes").unwrap()
    }

    #[cfg(test)]
    pub(crate) fn assert_conserved(&self) {
        let mut state = self.lock_state();
        let audit = Self::audit_gauges(&state);
        let ledger = Self::gauges(&mut state);
        for key in Self::GAUGE_KEYS {
            assert_eq!(
                number(&audit, key).unwrap(),
                number(&ledger, key).unwrap(),
                "retained ledger diverges from the audit on {key}"
            );
        }
        for reservable in state.reservables() {
            reservable.audit_reserved();
        }
        assert_eq!(
            state.recent_codex_raw_bytes,
            Self::audit_recent_codex_raw_bytes(&state),
            "recent codex raw bytes diverge from the audit"
        );
        assert_eq!(
            state.prepared_facts_lru.len(),
            state.prepared_facts.len(),
            "prepared facts lru index diverges from the cache"
        );
        for (identity, cached) in state.prepared_facts.iter() {
            assert!(
                state
                    .prepared_facts_lru
                    .contains(&(cached.last_used, *identity)),
                "prepared facts lru index misses a cached entry"
            );
        }
        assert_eq!(
            state.carried_expiry.key_bytes(),
            state.carried_expiry.audit_key_bytes(),
            "carried expiry tickets diverge from their lineage strings"
        );
        assert_eq!(
            state.locations_expiry.key_bytes(),
            state.locations_expiry.audit_key_bytes(),
            "location expiry tickets diverge from their session ids"
        );
        assert_eq!(
            state.deliveries_expiry.len(),
            state.deliveries.len(),
            "delivery deadline index diverges from the deliveries"
        );
        for (key, delivery) in state.deliveries.iter() {
            assert!(
                state.deliveries_expiry.contains(delivery.expires, key),
                "delivery deadline index misses a delivery"
            );
        }
    }

    fn foreground_admission(context: &Value) -> Result<bool, SnapshotError> {
        Ok(str_field(context, "admission")? == "hook"
            && context.get("work_class").and_then(Value::as_str) != Some("background"))
    }

    fn memory_cap(&self, context: &Value) -> Result<usize, SnapshotError> {
        Ok(if Self::foreground_admission(context)? {
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
        let bookkeeping = cfg!(debug_assertions).then(|| Self::bookkeeping_bytes(state));
        if number(&Self::gauges(state), "retained_total_accounted_bytes")?
            .saturating_add(additional)
            <= cap
        {
            state.ledger.shared.work().admitted(additional);
            return Ok(());
        }
        self.classified
            .lock()
            .expect("classified snapshots")
            .retain(|_, snapshot| Arc::strong_count(snapshot) > 1);
        let leased: HashSet<_> = state
            .leases
            .values()
            .map(|lease| lease.snapshot.id.clone())
            .collect();
        state.retain_latest(|_, snapshot| {
            leased.contains(&snapshot.id) || Arc::strong_count(snapshot) > 1
        });
        state.retain_carried(|_, carried| Arc::strong_count(carried) > 1);
        if number(&Self::gauges(state), "retained_total_accounted_bytes")?
            .saturating_add(additional)
            > cap
        {
            debug_assert_eq!(
                bookkeeping,
                Some(Self::bookkeeping_bytes(state)),
                "a refused admission moved retained bookkeeping"
            );
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "accounted storage admission exhausted",
            ));
        }
        state.ledger.shared.work().admitted(additional);
        Ok(())
    }

    fn lease_cap(&self, context: &Value) -> Result<usize, SnapshotError> {
        Ok(if Self::foreground_admission(context)? {
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
        token: String,
        pledged: usize,
    ) -> Result<Value, SnapshotError> {
        let cap = if Self::foreground_admission(context)? {
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
        let description = self.description(&snapshot, &classifier, &token, expires);
        let lease = Lease {
            claimant: str_field(context, "claimant")?.to_owned(),
            snapshot,
            classifier,
            expires,
            absolute_deadline,
            exposed: false,
            registry_generation: str_field(context, "registry_generation")?.to_owned(),
            registry,
        };
        let additional =
            (charged_bytes(&token, &lease) + state.leases.growth(1)).saturating_sub(pledged);
        self.admit_memory(state, context, additional)?;
        state.transient_bytes -= pledged;
        state.insert_lease(token, lease);
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
            "source_bytes": snapshot.stamp.size, "window_start": snapshot.window_start, "committed_bytes": snapshot.committed_bytes,
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
        let state = self.lock_state();
        self.lease(&state, handle, context).map(|_| ())
    }

    pub fn pin_scope(
        &self,
        handle: &Value,
        context: &Value,
    ) -> Result<(Arc<TranscriptSnapshot>, Value), SnapshotError> {
        self.authority(context, None)?;
        let (snapshot, description) = {
            let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
                    self.lock_state().counters[12] += 1;
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
                        let state = self.lock_state();
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
                        self.lock_state()
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
                    let mut state = self.lock_state();
                    if let Some(mut waiter) = state.waiters.get_mut(token) {
                        if bytes > waiter.limits.max_output_bytes {
                            return Err(SnapshotError::new(
                                Status::OutputLimit,
                                "reservation output budget exhausted",
                            ));
                        }
                        waiter.limits.max_output_bytes -= bytes;
                        data["reservation"]["remaining_work"]
                            .insert("max_output_bytes", json!(waiter.limits.max_output_bytes));
                    };
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
        let mut leases = Vec::new();
        if may_issue {
            for handle in handles {
                if let Some(mut lease) = state.leases.get_mut(&handle) {
                    if lease.claimant == claimant && !lease.exposed {
                        lease.exposed = true;
                        leases.push(handle);
                    }
                }
            }
        }
        if leases.is_empty() && cursor.is_none() {
            state.remove_delivery(&key);
            return;
        }
        if state.deliveries.len() >= self.config.leases.saturating_mul(4) {
            state.evict_earliest_delivery();
        }
        state.insert_delivery(
            Arc::from(key),
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
        let delivery = self.lock_state().remove_delivery(&key);
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
        let foreign = state
            .labels
            .get(token)
            .is_some_and(|slot| slot.preparation.binding().claimant != claimant)
            || state
                .graphs
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .prepared_builds
                .get(token)
                .is_some_and(|cursor| cursor.claimant != claimant)
            || state
                .prepared_queries
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
        let mut released = state.remove_label(token).is_some();
        released |= state.projections.remove(token).is_some();
        if let Some(query) = state.prepared_queries.remove(token) {
            if let Some(pending) = query.pending {
                state.waiters.remove(&pending.token);
            }
            released = true;
        }
        if let Some(build) = state.remove_prepared_build(token) {
            Self::release_prepared_build_state(&mut state, &build);
            released = true;
        }
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
        let mut state = self.lock_state();
        if state
            .prepared_queries
            .get(token)
            .is_some_and(|query| query.claimant == claimant)
        {
            if let Some(query) = state.prepared_queries.remove(token) {
                if let Some(pending) = query.pending {
                    state.waiters.remove(&pending.token);
                }
            }
        }
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
                    let state = self.lock_state();
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
                    let mut state = self.lock_state();
                    Self::prune(&mut state);
                    (
                        state.waiters.get(cursor).cloned(),
                        state.projections.get(cursor).cloned(),
                    )
                };
                let discovery = {
                    let mut state = self.lock_state();
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
                    let mut state = self.lock_state();
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
                    let mut state = self.lock_state();
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
                let prepared_build = {
                    let mut state = self.lock_state();
                    if state.prepared_builds.get(cursor).is_some_and(|build| {
                        build.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared build claimant differs",
                        ));
                    }
                    state.remove_prepared_build(cursor)
                };
                if let Some(mut build) = prepared_build {
                    if build.context["authority"] != context["authority"]
                        || build.context["admission"] != context["admission"]
                        || build.context["registry_generation"] != context["registry_generation"]
                    {
                        Self::release_prepared_build_state(&mut self.lock_state(), &build);
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared build context differs",
                        ));
                    }
                    build.context = context.clone();
                    return self.prepare_graph_step(cursor, build, cancel, usage);
                }
                let prepared_query = {
                    let mut state = self.lock_state();
                    if state.prepared_queries.get(cursor).is_some_and(|query| {
                        query.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared query claimant differs",
                        ));
                    }
                    state.prepared_queries.remove(cursor)
                };
                if let Some(query) = prepared_query {
                    return self.prepared_query_page(cursor, query, context, cancel, usage);
                }
                if let Some((claimant, _)) = self
                    .lock_state()
                    .expired_prepared_queries
                    .get(cursor)
                    .cloned()
                {
                    if claimant != str_field(context, "claimant")? {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared query claimant differs",
                        ));
                    }
                    return Err(SnapshotError::new(
                        Status::Deadline,
                        "prepared query deadline expired",
                    ));
                }
                let graph = {
                    let mut state = self.lock_state();
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
                    if let Some(mut current) = self.lock_state().waiters.get_mut(cursor) {
                        current.context = context.clone();
                    }
                    return self.advance(cursor, waiter, None, cancel, usage);
                }
                if let Some(projection) = projection {
                    if projection.claimant != str_field(context, "claimant")? {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    self.lock_state().projections.remove(cursor);
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
                let mut state = self.lock_state();
                let lease = self.lease(&state, handle, context)?;
                let classifier = lease.classifier.clone();
                let absolute_deadline = lease.absolute_deadline;
                let current_expiry = lease.expires;
                match operation {
                    "retain" => Ok((
                        self.issue(
                            &mut state,
                            snapshot,
                            classifier,
                            context,
                            absolute_deadline,
                            self.token("lease"),
                            0,
                        )?,
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
                let mut state = self.lock_state();
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
                            state.retain_labels(|_, slot| {
                                slot.source_handle["lease_id"].as_str() != Some(token)
                            });
                        }
                        released
                    }
                    "graph" => {
                        if state.prepared_graphs.get(token).is_some_and(|graph| {
                            let graph = graph.lock().expect("prepared graph");
                            graph.claimant != claimant
                                || graph.admission != str_field(context, "admission").unwrap_or("")
                                || graph.registry_generation
                                    != str_field(context, "registry_generation").unwrap_or("")
                                || graph.authority != context["authority"]
                        }) {
                            return Err(SnapshotError::new(
                                Status::StaleHandle,
                                "prepared graph claimant differs",
                            ));
                        }
                        let released = state.remove_prepared_graph(token).is_some();
                        let discarded: Vec<_> = state
                            .prepared_queries
                            .iter()
                            .filter(|(_, cursor)| cursor.graph_id == token)
                            .map(|(query_token, _)| query_token.clone())
                            .collect();
                        for query_token in discarded {
                            if let Some(query) = state.prepared_queries.remove(&query_token) {
                                if let Some(pending) = query.pending {
                                    state.waiters.remove(&pending.token);
                                }
                            }
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
                let mut state = self.lock_state();
                Self::prune(&mut state);
                Ok((
                    json!({"kind": "stats", "counters": usage_value(&state.counters), "gauges": Self::gauges(&mut state)}),
                    None,
                    None,
                ))
            }
            "prepare_graph" => self.prepare_graph(request, context, cancel, usage),
            "query_graph" => self.query_graph(request, context, cancel, usage),
            "warm_registered" => self.warm_registered(request, context, cancel, usage),
            "warm_root" => self.warm_root(request, context, cancel, usage),
            "query" if Self::is_graph_request(request) => {
                self.graph(request, context, cancel, usage)
            }
            "query" | "capture" | "activity_probe" | "hydrate" | "mine" => {
                self.project_request(request, context, cancel, limits(request)?, 0)
            }
            "discover" | "resolve" => self.discover(request, context, cancel, usage),
            "locate" => self.locate(request, context, cancel, usage),
            "tail" => self.tail(request, context, cancel, usage),
            _ => Err(invalid("unsupported operation")),
        }
    }

    fn tail(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let limits = limits(request)?;
        cancel.check(limits.deadline_unix_ms)?;
        let count = number(request, "count")?;
        if count == 0
            || count > limits.max_events
            || count > limits.max_items
            || count > crate::snapshot_codec::MAX_RECORDS
        {
            return Err(invalid(
                "tail count must be positive and within the event, item, and page budgets",
            ));
        }
        let path = std::fs::canonicalize(str_field(request, "path")?).map_err(io_error)?;
        self.authority(context, Some(&path))?;
        usage[0] += 1;
        let mut file = File::open(&path).map_err(io_error)?;
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
        let budget = limits.max_source_read_bytes.min(stamp.size as usize);
        let mut reservation = self.reserve_projection(context, 0)?;
        let mut pending: Vec<u8> = Vec::new();
        let mut pending_start = stamp.size;
        let mut cut = 0usize;
        let mut read = 0usize;
        let mut provider = None;
        let mut lines: Vec<(u64, Vec<Entry>)> = Vec::new();
        let mut events = 0usize;
        let mut exhausted = false;
        loop {
            while events < count {
                cancel.check(limits.deadline_unix_ms)?;
                let Some(newline) = memchr::memrchr(b'\n', &pending[..cut]) else {
                    break;
                };
                let start = newline + 1;
                if let Some(entries) = self.tail_line(&pending[start..cut], &mut provider, usage)? {
                    events += entries.len();
                    lines.push((pending_start + start as u64, entries));
                }
                cut = newline;
            }
            if events >= count {
                break;
            }
            if pending_start == 0 {
                if let Some(entries) = self.tail_line(&pending[..cut], &mut provider, usage)? {
                    lines.push((0, entries));
                }
                break;
            }
            let step = self
                .config
                .read_step
                .min(budget - read)
                .min(pending_start as usize);
            if step == 0 {
                exhausted = true;
                break;
            }
            self.extend_projection_reservation(&mut reservation, context, step.saturating_mul(6))?;
            pending.truncate(cut);
            let mut block = vec![0; step];
            file.seek(SeekFrom::Start(pending_start - step as u64))
                .map_err(io_error)?;
            self.before_source_read();
            file.read_exact(&mut block).map_err(io_error)?;
            usage[1] += step as u64;
            read += step;
            block.extend_from_slice(&pending);
            pending = block;
            pending_start -= step as u64;
            cut = pending.len();
        }
        if !Self::matches_prefix(SourceStamp::of(&file.metadata().map_err(io_error)?), stamp) {
            return Err(SnapshotError::new(
                Status::Changed,
                "source changed while reading",
            ));
        }
        let output_limit = limits
            .max_output_bytes
            .min(self.config.output)
            .min(MAX_DATA_BYTES);
        let envelope = encoded_size(
            &json!({"kind":"tail","record_schema":"cc-transcript.event/1","records_json":[],
                "source_bytes":stamp.size,"window_start_byte":stamp.size}),
            output_limit,
        )?;
        let mut selected: Vec<(u64, &Entry)> = Vec::new();
        let mut output = envelope;
        let mut clipped = false;
        for (offset, entry) in lines
            .iter()
            .flat_map(|(offset, entries)| entries.iter().rev().map(move |entry| (*offset, entry)))
            .take(count)
        {
            let wire = crate::snapshot_codec::EventWire::new(count - 1, entry);
            let bytes = match crate::snapshot_codec::encode(&wire, output_limit) {
                Ok(record) => encoded_size(&json!(record), output_limit)? + 1,
                Err(error) if error.status == Status::OutputLimit && !selected.is_empty() => {
                    clipped = true;
                    break;
                }
                Err(error) => return Err(error),
            };
            if output + bytes > output_limit {
                clipped = true;
                break;
            }
            output += bytes;
            selected.push((offset, entry));
        }
        selected.reverse();
        let window_start = selected.first().map_or(stamp.size, |(offset, _)| *offset);
        let records = selected
            .iter()
            .enumerate()
            .map(|(index, (_, entry))| {
                crate::snapshot_codec::encode(
                    &crate::snapshot_codec::EventWire::new(index, entry),
                    output_limit,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let reason = if selected.len() >= count {
            None
        } else if clipped {
            Some("tail output budget exhausted".to_owned())
        } else if exhausted {
            Some("tail read budget exhausted".to_owned())
        } else {
            None
        };
        Ok((
            json!({"kind":"tail","record_schema":"cc-transcript.event/1","records_json":records,
                "source_bytes":stamp.size,"window_start_byte":window_start}),
            None,
            reason,
        ))
    }

    fn tail_line(
        &self,
        line: &[u8],
        provider: &mut Option<Provider>,
        usage: &mut [u64; 18],
    ) -> Result<Option<Vec<Entry>>, SnapshotError> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        if line.len() > self.config.entry {
            return Err(SnapshotError::new(
                Status::EntryLimit,
                "source entry exceeds owner bound",
            ));
        }
        if provider.unwrap_or_else(|| sniff_provider(line)) == Provider::Codex {
            return Err(invalid("tail requires a Claude source"));
        }
        let mut entries = Vec::new();
        crate::parse::parse_line(line, &mut entries, &|_| true)
            .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
        usage[2] += line.len() as u64;
        usage[3] += 1;
        if entries.is_empty() {
            return Ok(None);
        }
        *provider = Some(Provider::Claude);
        Ok(Some(entries))
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
        let window = tail_bytes(request)?;
        let stamp = SourceStamp::of(&metadata).windowed(window);
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
            let mut state = self.lock_state();
            Self::prune(&mut state);
            let cached = state
                .latest
                .get(&stamp.identity)
                .filter(|snapshot| snapshot.stamp == stamp)
                .cloned();
            if cached.is_some() {
                usage[7] += 1;
            }
            let (slot, created) = if let Some(slot) = state.loads.get(&stamp.identity) {
                if !Self::matches_prefix(stamp, slot.stamp)
                    && !Self::matches_prefix(slot.stamp, stamp)
                {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "source changed during shared preparation",
                    ));
                }
                usage[8] += 1;
                (Arc::clone(slot), false)
            } else {
                let cap = if Self::foreground_admission(context)? {
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
                        old.event_count > 0
                            && old.stamp.size < stamp.size
                            && (old.provider == Provider::Claude
                                || old.provider == Provider::Codex && old.codex_raw.is_some())
                    })
                    .cloned();
                let previous_index_compatible = previous.as_ref().is_some_and(|previous| {
                    state
                        .generations
                        .get(&snapshot_key(previous))
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
                    attached: AtomicBool::new(false),
                    deadline: AtomicU64::new(now + self.config.preparation),
                    work: Mutex::new(Load {
                        file,
                        path,
                        offset: 0,
                        pending: Vec::new(),
                        pending_start: 0,
                        provider: None,
                        chunks: Vec::new(),
                        codex_raw: None,
                        codex_append: None,
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
                        window_scanned: 0,
                        window_start: (stamp.identity.window_base == 0).then_some(0),
                        fence: Vec::new(),
                        committed: 0,
                        provisional: false,
                        result: cached,
                        failure: None,
                    }),
                });
                state.insert_load(stamp.identity, Arc::clone(&slot));
                (slot, true)
            };
            if state.waiters.len() >= self.lease_cap(context)? {
                if created {
                    state.remove_load(&stamp.identity);
                }
                return Err(SnapshotError::new(
                    Status::LeaseLimit,
                    "reservation admission exhausted",
                ));
            }
            let deadline = limits
                .deadline_unix_ms
                .min(slot.deadline.load(Ordering::Acquire));
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
                used_source_bytes: 0,
                used_events: 0,
                stage: None,
                windowed: window.is_some(),
                busy: false,
            };
            let additional = charged_bytes(&cursor, &waiter) + state.waiters.growth_for(&cursor);
            if let Err(error) = self.admit_memory(&mut state, context, additional) {
                if created {
                    state.remove_load(&stamp.identity);
                }
                return Err(error);
            }
            state.insert_waiter(cursor.clone(), waiter.clone());
            waiter
        };
        self.advance(&cursor, waiter, None, cancel, usage)
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
            "absolute_deadline_unix_ms": waiter.deadline, "remaining_work": WorkLimits {
                max_read_bytes: waiter.limits.max_read_bytes.saturating_sub(waiter.used_bytes),
                max_source_read_bytes: waiter.limits.max_source_read_bytes.saturating_sub(waiter.used_source_bytes),
                max_events: waiter.limits.max_events.saturating_sub(waiter.used_events),
                ..waiter.limits
            }.to_json()}}),
            Some(token.to_owned()),
            Some(reason.to_owned()),
        )
    }

    fn advance(
        &self,
        token: &str,
        mut waiter: Waiter,
        bound: Option<&WorkLimits>,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        if let Err(error) = cancel.check(waiter.deadline) {
            self.lock_state().waiters.remove(token);
            return Err(error);
        }
        {
            let mut state = self.lock_state();
            let mut current = state
                .waiters
                .get_mut(token)
                .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "reservation released"))?;
            if current.busy {
                return Ok(self.loading(token, &current, "reservation preparation is running"));
            }
            waiter = current.clone();
            current.busy = true;
        }
        if let Some(bound) = bound {
            waiter.limits.max_source_read_bytes =
                waiter.used_source_bytes + bound.max_source_read_bytes;
            waiter.limits.max_events = waiter.used_events + bound.max_events;
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
        let mut lease = None;
        let stepped = if load.result.is_none() {
            let read_bound = self.config.read_step.min(
                waiter
                    .limits
                    .max_source_read_bytes
                    .saturating_sub(waiter.used_source_bytes),
            );
            let events_bound = self
                .config
                .event_step
                .min(waiter.limits.max_events.saturating_sub(waiter.used_events));
            if read_bound == 0 && load.offset < slot.stamp.size {
                self.lock_state().waiters.remove(token);
                return Err(source_read_limit());
            }
            if events_bound == 0
                && (load.offset < slot.stamp.size
                    || !load.pending.is_empty()
                    || load.indexed < load.count)
            {
                self.lock_state().waiters.remove(token);
                return Err(SnapshotError::new(
                    Status::SourceLimit,
                    "cumulative preparation work budget exhausted",
                ));
            }
            let decode_source = if load.provider == Some(Provider::Codex) {
                slot.stamp.size as usize
            } else {
                self.config
                    .entry
                    .min(load.pending.len().saturating_add(read_bound))
            };
            let reservation = read_bound.saturating_add(decode_source.saturating_mul(4));
            {
                let mut state = self.lock_state();
                self.admit_memory(&mut state, &waiter.context, reservation)?;
                state.add_load_charge(&slot, reservation);
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
            waiter.used_source_bytes += (usage[1] - before_bytes) as usize;
            waiter.used_events += (usage[3] - before_events) as usize
                + if checked_prefix == load.prefix_checked {
                    load.indexed.saturating_sub(before_indexed)
                } else {
                    0
                };
            Some(result)
        } else {
            None
        };
        let unpublished = load.result.as_ref().is_some_and(|snapshot| {
            !self
                .lock_state()
                .generations
                .contains_key(&snapshot_key(snapshot))
        });
        if stepped.is_some() || unpublished {
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
                    + load.codex_raw.as_ref().map_or(0, |raw| {
                        if load
                            .previous
                            .as_ref()
                            .and_then(|previous| previous.codex_raw.as_ref())
                            .is_some_and(|previous| Arc::ptr_eq(previous, raw))
                        {
                            0
                        } else {
                            raw.capacity()
                        }
                    })
                    + load.codex_append.as_ref().map_or(0, |index| {
                        if load
                            .previous
                            .as_ref()
                            .and_then(|previous| previous.codex_append.as_ref())
                            .is_some_and(|previous| Arc::ptr_eq(previous, index))
                        {
                            0
                        } else {
                            index.accounted_bytes()
                        }
                    })
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
            let pledge = generation
                .as_ref()
                .map(|_| {
                    let token = self.token("lease");
                    Lease::pledge(&token, &waiter.context).map(|bytes| (token, bytes))
                })
                .transpose()?;
            {
                let mut state = self.lock_state();
                let generation = load
                    .result
                    .as_ref()
                    .zip(generation)
                    .filter(|(snapshot, _)| {
                        !state.generations.contains_key(&snapshot_key(snapshot))
                    });
                let pledge = pledge.map(|(token, bytes)| (token, bytes + state.leases.growth(1)));
                let additional = generation.as_ref().map_or(0, |(snapshot, record)| {
                    let key = snapshot_key(snapshot);
                    state.admission(&key, record, record.anchors())
                        + state.generations.growth_for(&key)
                        + state.escape_growth(&snapshot.chunks)
                }) + pledge.as_ref().map_or(0, |(_, bytes)| *bytes)
                    + pending_charge;
                self.admit_memory(
                    &mut state,
                    &waiter.context,
                    additional.saturating_sub(slot.ledgered_bytes()),
                )?;
                if let Some((snapshot, record)) = generation {
                    state.register_generation(snapshot, record);
                    state.escape_chunks(&snapshot.chunks);
                }
                if let Some((_, bytes)) = &pledge {
                    state.transient_bytes += bytes;
                }
                state.set_load_charge(&slot, pending_charge);
                lease = pledge
                    .map(|(token, bytes)| (token, ProjectionReservation { store: self, bytes }));
            }
        }
        if let Some(Err(error)) = stepped {
            if !matches!(
                error.status,
                Status::Cancelled | Status::Deadline | Status::Incomplete
            ) {
                load.failure = Some(SnapshotError::new(error.status, &error.reason));
            }
            self.lock_state().waiters.remove(token);
            return Err(error);
        }
        if let Err(error) = cancel.check(waiter.deadline) {
            self.lock_state().waiters.remove(token);
            return Err(error);
        }
        if let Some(snapshot) = &load.result {
            let snapshot = Arc::clone(snapshot);
            drop(load);
            if waiter.windowed && snapshot.provider == Provider::Codex {
                self.lock_state().waiters.remove(token);
                return Err(invalid("tail_bytes requires a Claude source"));
            }
            let source = Arc::clone(&snapshot);
            let mut state = self.lock_state();
            let fresh = !state
                .latest
                .get(&slot.stamp.identity)
                .is_some_and(|old| old.id == snapshot.id);
            if fresh {
                let escaping = state.escape_growth(&snapshot.chunks);
                if let Err(error) = self.admit_memory(&mut state, &waiter.context, escaping) {
                    state.waiters.remove(token);
                    return Err(error);
                }
                state.escape_chunks(&snapshot.chunks);
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
            waiter.stage = classified.stage;
            let Some(snapshot) = classified.snapshot else {
                waiter.expires = (now_ms() + self.config.ttl).min(waiter.deadline);
                waiter.busy = false;
                let mut state = self.lock_state();
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
            let (lease_token, mut pledge) = match lease.take() {
                Some((lease_token, pledge)) => (lease_token, Some(pledge)),
                None => (self.token("lease"), None),
            };
            let mut state = self.lock_state();
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
                lease_token,
                pledge.as_ref().map_or(0, |pledge| pledge.bytes),
            )?;
            if let Some(pledge) = &mut pledge {
                pledge.bytes = 0;
            }
            if fresh {
                let latest = state.latest.growth_for(&slot.stamp.identity);
                if self
                    .admit_memory(&mut state, &waiter.context, latest)
                    .is_ok()
                    && state.insert_latest(slot.stamp.identity, source).is_some()
                {
                    usage[10] += 1;
                }
            }
            state.waiters.remove(token);
            Self::prune(&mut state);
            Ok((data, None, None))
        } else {
            drop(load);
            waiter.expires = (now_ms() + self.config.ttl).min(waiter.deadline);
            let mut state = self.lock_state();
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
        let current = current.viewed_as(pinned);
        current == pinned || current.identity == pinned.identity && current.size > pinned.size
    }

    fn extends_prefix(
        &self,
        snapshot: &TranscriptSnapshot,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 18],
    ) -> Result<bool, SnapshotError> {
        let Some((end, fence)) = snapshot.prefix_fence() else {
            return Ok(false);
        };
        let mut file = File::open(&snapshot.canonical_path).map_err(io_error)?;
        usage[0] += 1;
        if !Self::matches_prefix(
            SourceStamp::of(&file.metadata().map_err(io_error)?),
            snapshot.stamp,
        ) {
            return Ok(false);
        }
        if fence.len() > remaining.max_source_read_bytes {
            return Err(source_read_limit());
        }
        let mut current = vec![0; fence.len()];
        file.seek(SeekFrom::Start(end - fence.len() as u64))
            .map_err(io_error)?;
        file.read_exact(&mut current).map_err(io_error)?;
        usage[1] += current.len() as u64;
        remaining.max_source_read_bytes -= current.len();
        Ok(current == fence)
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
                return Err(source_read_limit());
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
                let (prior_end, prior_fence) =
                    previous.prefix_fence().expect("cached codex source");
                let count = prior_fence
                    .len()
                    .saturating_sub(load.prefix_fence.len())
                    .min(read_bound);
                if count == 0 && prior_fence.len() > load.prefix_fence.len() {
                    return Err(source_read_limit());
                }
                load.file
                    .seek(SeekFrom::Start(
                        prior_end - prior_fence.len() as u64 + load.prefix_fence.len() as u64,
                    ))
                    .map_err(io_error)?;
                let start = load.prefix_fence.len();
                load.prefix_fence.resize(start + count, 0);
                self.before_source_read();
                load.file
                    .read_exact(&mut load.prefix_fence[start..])
                    .map_err(io_error)?;
                usage[1] += count as u64;
                if load.prefix_fence.len() < prior_fence.len() {
                    return Ok(());
                }
                let fence = std::mem::take(&mut load.prefix_fence);
                if fence == prior_fence {
                    if previous.provider == Provider::Codex {
                        load.pending = Vec::new();
                        load.offset = previous.stamp.size;
                        load.pending_start = previous.stamp.size;
                        load.provider = Some(Provider::Codex);
                        load.session_id = Some(previous.session_id.clone());
                        load.chunks = previous.chunks.clone();
                        load.count = previous.event_count;
                        load.codex_raw = previous.codex_raw.clone();
                        load.codex_append = previous.codex_append.clone();
                        if load.previous_index_compatible {
                            load.activity = previous.activity.as_ref().clone();
                            load.indexed = previous.event_count;
                        }
                    } else {
                        let keep = previous
                            .chunks
                            .len()
                            .saturating_sub(usize::from(previous.provisional_tail));
                        load.chunks = previous.chunks[..keep].to_vec();
                        load.count = load.chunks.iter().map(|chunk| chunk.entries.len()).sum();
                        load.committed = previous.committed_bytes;
                        load.offset = previous.committed_bytes;
                        load.pending_start = previous.committed_bytes;
                        load.window_start = Some(previous.window_start);
                        load.provider = Some(Provider::Claude);
                        load.session_id = Some(previous.session_id.clone());
                        if !previous.provisional_tail && load.previous_index_compatible {
                            load.activity = previous.activity.as_ref().clone();
                            load.indexed = previous.event_count;
                        }
                        load.fence = fence;
                    }
                    load.prefix_checked = true;
                } else {
                    load.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
                    load.previous = None;
                    usage[4] += 1;
                }
                return Ok(());
            }
        }
        if load.window_start.is_none() {
            let unscanned = slot.stamp.identity.window_base - load.window_scanned;
            let located = if unscanned == 0 {
                Some(0)
            } else {
                if load.window_scanned > self.config.entry as u64 {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                let count = (read_bound as u64).min(unscanned);
                if count == 0 {
                    return Err(source_read_limit());
                }
                load.file
                    .seek(SeekFrom::Start(unscanned - count))
                    .map_err(io_error)?;
                let mut block = vec![0; count as usize];
                self.before_source_read();
                load.file.read_exact(&mut block).map_err(io_error)?;
                usage[1] += count;
                load.window_scanned += count;
                memchr::memrchr(b'\n', &block).map(|newline| unscanned - count + newline as u64 + 1)
            };
            let Some(start) = located else {
                return Ok(());
            };
            load.window_start = Some(start);
            load.offset = start;
            load.pending_start = start;
            load.committed = start;
            load.file.seek(SeekFrom::Start(start)).map_err(io_error)?;
            return Ok(());
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
                if slot.stamp.identity.window_base > 0 {
                    return Err(invalid("tail_bytes requires a Claude source"));
                }
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
                self.lower_codex_source(slot, load, lowering_events, usage)?;
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
                return Err(source_read_limit());
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
            ledger: LedgerHook::default(),
            id: self.token("snapshot"),
            canonical_path: load.path.clone(),
            stamp: slot.stamp,
            provider: load.provider.unwrap_or(Provider::Claude),
            session_id,
            chunks: load.chunks.clone(),
            activity: Arc::new(load.activity.clone()),
            window_start: load.window_start.expect("located window"),
            committed_bytes: load.committed,
            provisional_tail: load.provisional,
            fence: load.fence.clone(),
            event_count: load.count,
            codex_raw: load.codex_raw.clone(),
            codex_append: load.codex_append.clone(),
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

    fn graph_exposes_members(graph: &GraphCursor) -> bool {
        matches!(
            graph.request["query"]["kind"].as_str(),
            Some("sidechain_membership" | "direct_sidechains")
        )
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
        let identity = root.stamp.identity.file();
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
            pending_records: VecDeque::new(),
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
                Self::rollback_graph_page(&mut self.lock_state(), &mut graph);
                return Err(error);
            }
        };
        let output = match encoded_size(&data, graph.remaining.max_output_bytes.min(MAX_DATA_BYTES))
        {
            Ok(bytes) => bytes,
            Err(error) => {
                Self::rollback_graph_page(&mut self.lock_state(), &mut graph);
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
                    .pending_records
                    .iter()
                    .map(|(_, record)| record.capacity())
                    .sum::<usize>()
                + graph.listing.as_ref().map_or(0, |listing| {
                    listing
                        .children
                        .iter()
                        .map(|path| path.as_os_str().len())
                        .sum::<usize>()
                });
        }
        let mut state = self.lock_state();
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
        let identity = snapshot.stamp.identity.file();
        if !graph.seen.insert(identity)
            && graph.request["query"]["kind"].as_str() != Some("direct_sidechains")
        {
            self.lock_state()
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
        if !Self::graph_exposes_members(graph) {
            self.lock_state()
                .leases
                .remove(str_field(
                    &graph.nodes.last().expect("added graph node").description["handle"],
                    "lease_id",
                )?)
                .expect("validated internal graph lease");
        }
        if graph.request["query"]["kind"].as_str() != Some("direct_sidechains") {
            graph.tasks.push(GraphTask::List {
                parent: path,
                depth: depth + 1,
            });
        }
        Ok(())
    }

    fn renew_graph_members(&self, graph: &mut GraphCursor) -> Result<(), SnapshotError> {
        let mut state = self.lock_state();
        self.lease(&state, &graph.root_handle, &graph.context)?;
        if !Self::graph_exposes_members(graph) {
            return Ok(());
        }
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

    fn advance_graph_source(
        &self,
        graph: &mut GraphCursor,
        pending: GraphPending,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
        work: &mut usize,
        work_stop: usize,
    ) -> Result<bool, SnapshotError> {
        self.authority(
            &graph.context,
            Some(&std::fs::canonicalize(&pending.path).map_err(io_error)?),
        )?;
        let token = pending.token.clone();
        graph.pending = Some(pending);
        loop {
            if *work >= work_stop {
                return Ok(false);
            }
            *work += 1;
            let waiter = {
                let mut state = self.lock_state();
                let mut waiter = state.waiters.get_mut(&token).ok_or_else(|| {
                    SnapshotError::new(Status::StaleCursor, "graph source reservation expired")
                })?;
                waiter.context = graph.context.clone();
                waiter.clone()
            };
            let before_bytes = usage[1];
            let before_events = usage[3];
            let outcome = self.advance(&token, waiter, Some(&graph.remaining), cancel, usage)?;
            graph.remaining.max_source_read_bytes = graph
                .remaining
                .max_source_read_bytes
                .saturating_sub((usage[1] - before_bytes) as usize);
            graph.remaining.max_events = graph
                .remaining
                .max_events
                .saturating_sub((usage[3] - before_events) as usize);
            if outcome.1.is_some() {
                continue;
            }
            let pending = graph.pending.take().expect("completed graph source");
            self.graph_add_source(
                graph,
                pending.path,
                pending.depth,
                pending.spawned_by,
                &outcome.0,
            )?;
            return Ok(true);
        }
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
        let work_stop = self.config.event_step.min(self.config.page_items);
        let mut examined = 0usize;
        if let Some(pending) = graph.pending.take() {
            if !self.advance_graph_source(
                graph,
                pending,
                cancel,
                usage,
                &mut examined,
                work_stop,
            )? {
                return Ok(GraphYield::Pending(Value::new_null()));
            }
        }
        while !graph.prepared && examined < work_stop {
            cancel.check(graph.remaining.deadline_unix_ms)?;
            if let Some(mut listing) = graph.listing.take() {
                loop {
                    if examined >= work_stop {
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
                    examined += 1;
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
                        "limits":bounds.to_json()});
                    let before_bytes = usage[1];
                    let before_events = usage[3];
                    let outcome = self.acquire(&acquire, &graph.context, cancel, usage)?;
                    graph.remaining.max_source_read_bytes = graph
                        .remaining
                        .max_source_read_bytes
                        .saturating_sub((usage[1] - before_bytes) as usize);
                    graph.remaining.max_events = graph
                        .remaining
                        .max_events
                        .saturating_sub((usage[3] - before_events) as usize);
                    if let Some(token) = outcome.1 {
                        if !self.advance_graph_source(
                            graph,
                            GraphPending {
                                token,
                                path,
                                depth,
                                spawned_by,
                            },
                            cancel,
                            usage,
                            &mut examined,
                            work_stop,
                        )? {
                            return Ok(GraphYield::Pending(Value::new_null()));
                        }
                    } else {
                        self.graph_add_source(graph, path, depth, spawned_by, &outcome.0)?;
                    }
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
        let mut projected = 0usize;
        while graph.projection_at < total || !graph.pending_records.is_empty() {
            cancel.check(graph.remaining.deadline_unix_ms)?;
            if boolean && projected >= self.config.page_items {
                return Ok(GraphYield::Pending(Value::new_null()));
            }
            if records.len() >= page_limit {
                break;
            }
            let (index, record) = if let Some(pending) = graph.pending_records.pop_front() {
                pending
            } else {
                let position = if reverse {
                    total - 1 - graph.projection_at
                } else {
                    graph.projection_at
                };
                let index = position + usize::from(membership || boolean);
                let node = &graph.nodes[index];
                if membership {
                    self.validate_scope(&node.description["handle"], &graph.context)?;
                }
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
                    if boolean {
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
                        projected += 1;
                        if projection.data["value"].as_bool() == Some(true) {
                            return Ok(GraphYield::Complete(projection.data));
                        }
                        if graph.projection_at >= total {
                            break;
                        }
                        continue;
                    }
                    let (member_records, read_bytes, events) =
                        crate::snapshot_projection::predicate_input_records(
                            &node.snapshot,
                            &request["view"]["selectors"],
                            &bounds,
                            cancel,
                        )?;
                    graph.remaining.max_read_bytes =
                        graph.remaining.max_read_bytes.saturating_sub(read_bytes);
                    graph.remaining.max_events = graph.remaining.max_events.saturating_sub(events);
                    graph.projection_at += 1;
                    let mut member_records =
                        member_records.into_iter().map(|record| (index, record));
                    let first = member_records.next().expect("predicate member record");
                    graph.pending_records.extend(member_records);
                    first
                }
            };
            let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)?;
            if output_bytes.saturating_add(record_bytes)
                > graph.remaining.max_output_bytes.min(MAX_DATA_BYTES)
            {
                graph.pending_records.push_front((index, record));
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
        if graph.projection_at >= total && graph.pending_records.is_empty() {
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
                let mut state = self.lock_state();
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
            let mut state = self.lock_state();
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
        let mut state = self.lock_state();
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
        let mut state = self.lock_state();
        Self::prune(&mut state);
        let remembered = || {
            entries
                .iter()
                .filter(|(id, path)| id.len() + path.as_os_str().len() <= 8192)
        };
        let added = remembered()
            .map(|(id, path)| 2 * id.len() + path.as_os_str().len() + size_of::<LocatedPath>())
            .sum::<usize>();
        while state.locations.len() + entries.len() > 4096 && state.evict_oldest_location() {}
        let (fresh, pushed) = remembered().fold((0, 0), |(fresh, pushed), (id, _)| {
            (
                fresh + usize::from(!state.locations.contains_key(id)),
                pushed + 1,
            )
        });
        let additional =
            added + state.locations.growth(fresh) + state.locations_expiry.growth(pushed);
        if self.admit_memory(&mut state, context, additional).is_err() {
            return;
        }
        state.locations.reserve(fresh);
        state.locations_expiry.reserve(pushed);
        let expires = now_ms().saturating_add(self.config.ttl.saturating_mul(10));
        for (id, path) in remembered() {
            state.insert_location(
                id.clone(),
                LocatedPath {
                    path: path.clone(),
                    expires,
                },
            );
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
            let state = self.lock_state();
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
        let mut state = self.lock_state();
        Self::prune(&mut state);
        if state.locates.len() >= self.lease_cap(&cursor.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "location cursor admission exhausted",
            ));
        }
        let additional = cursor.accounted_bytes() + state.locates.growth_for(token);
        self.admit_memory(&mut state, &cursor.context, additional)?;
        cursor.expires = (now_ms() + self.config.ttl).min(cursor.limits.deadline_unix_ms);
        state.locates.reserve_for(token);
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
            let mut state = self.lock_state();
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
                    .lock_state()
                    .waiters
                    .get(pending)
                    .cloned()
                    .ok_or_else(|| {
                        SnapshotError::new(Status::StaleCursor, "resolution reservation expired")
                    })?;
                self.authority(&cursor.context, Some(path))?;
                if let Some(mut current) = self.lock_state().waiters.get_mut(pending) {
                    current.context = cursor.context.clone();
                }
                self.advance(pending, waiter, Some(&cursor.remaining), cancel, usage)?
            } else {
                let mut acquire = cursor.request.clone();
                acquire.insert("operation", json!("acquire"));
                acquire.insert("path", json!(path.to_string_lossy().as_ref()));
                let bound = cursor.remaining;
                acquire.insert("limits", bound.to_json());
                self.acquire(&acquire, &cursor.context, cancel, usage)?
            };
            cursor.remaining.max_source_read_bytes = cursor
                .remaining
                .max_source_read_bytes
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
                    self.lock_state()
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
        let mut state = self.lock_state();
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

include!("snapshot_prepared_service.rs");
include!("snapshot_root_warm.rs");
include!("snapshot_codex_append.rs");

#[cfg(test)]
#[path = "snapshot_regressions.rs"]
mod regression_tests;

#[cfg(test)]
#[path = "snapshot_ledger_tests.rs"]
mod ledger_tests;

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
            "deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":1000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
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

    fn finish_prepared(store: &NativeStore, mut response: Value, context: &Value) -> Value {
        for _ in 0..4096 {
            if response["status"].as_str() != Some("incomplete") {
                return response;
            }
            let Some(cursor) = response["cursor"].as_str() else {
                return response;
            };
            response = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                context,
                &Cancellation::default(),
            );
        }
        panic!("prepared graph did not finish");
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
    fn projection_reservations_preserve_exact_admission_caps_and_release_capacity() {
        for background in [false, true] {
            let store = store();
            let mut owner = context("a");
            if background {
                owner.insert("work_class", json!("background"));
            }
            let baseline = store.retained_accounted_bytes();
            let cap = if background {
                32 * 1024 * 1024 - 4096
            } else {
                32 * 1024 * 1024
            };
            let reservation = store.reserve_projection(&owner, cap - baseline).unwrap();
            assert_eq!(store.retained_accounted_bytes(), cap);
            let refused = store.reserve_projection(&owner, 1);
            assert!(matches!(refused, Err(error) if error.status == Status::RetainedLimit));
            assert_eq!(store.retained_accounted_bytes(), cap);
            drop(reservation);
            assert_eq!(store.retained_accounted_bytes(), baseline);
            let reused = store.reserve_projection(&owner, cap - baseline).unwrap();
            assert_eq!(store.retained_accounted_bytes(), cap);
            drop(reused);
            assert_eq!(store.retained_accounted_bytes(), baseline);
        }
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
        state.retain_latest(|_, _| false);
        NativeStore::prune(&mut state);
        assert!(
            number(
                &NativeStore::gauges(&mut state),
                "retained_entry_capacity_bytes"
            )
            .unwrap()
                > 0
        );
        drop(escaped);
        NativeStore::prune(&mut state);
        assert_eq!(
            number(
                &NativeStore::gauges(&mut state),
                "retained_entry_capacity_bytes"
            )
            .unwrap(),
            0
        );
        drop(state);
        store.assert_conserved();
    }

    #[test]
    fn budgets_and_authority_fail_explicitly() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let mut request = acquire(&source.path);
        request["limits"].insert("max_source_read_bytes", json!(10));
        let result = finish_prepared(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(result["status"].as_str(), Some("incomplete"));
        assert_eq!(result["reason"].as_str(), Some("source_read_limit"));
        assert!(result["cursor"].is_null());
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

    fn entry_charges(snapshot: &TranscriptSnapshot) -> Vec<usize> {
        snapshot
            .chunks
            .iter()
            .flat_map(|chunk| chunk.entry_charges.iter())
            .map(|charge| charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes)
            .collect()
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
                    Ok(vec![true; range.len()])
                }),
            )
            .unwrap();
        batches
    }

    fn classifier_acquire(path: &Path, id: &str, max_read_bytes: usize) -> Value {
        let mut request = acquire(path);
        request.insert("classifier", json!({"id":id,"version":"1"}));
        request["limits"].insert("max_read_bytes", json!(max_read_bytes));
        request
    }

    fn held_classifier_stages(store: &NativeStore) -> usize {
        store
            .state
            .lock()
            .unwrap()
            .classifier_stages
            .values()
            .filter(|slot| Arc::strong_count(slot) > 1)
            .count()
    }

    #[test]
    fn classifier_batches_shrink_to_the_read_budget_and_resume_where_they_stopped() {
        let source = Source::new(
            &(0..5)
                .map(|index| format!("{}\n", user(&index.to_string())))
                .collect::<String>(),
        );
        let store = store();
        let owner = context("a");
        let native = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let charges = entry_charges(&store.pin(handle(&native), &owner).unwrap());
        let batches = recording_classifier(&store, "budgeted");
        let budget = charges[..4].iter().sum::<usize>() - 1;
        let first = store.request(
            &classifier_acquire(&source.path, "budgeted", budget),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        assert_eq!(
            first["reason"].as_str(),
            Some("classifier preparation incomplete")
        );
        let resume = |response: &Value| {
            store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":response["cursor"]}),
                &owner,
                &Cancellation::default(),
            )
        };
        let second = resume(&first);
        assert_eq!(second["status"].as_str(), Some("incomplete"), "{second:?}");
        assert!(second["cursor"].as_str().is_some());
        let exhausted = resume(&second);
        assert_eq!(exhausted["status"].as_str(), Some("incomplete"));
        assert_eq!(
            exhausted["reason"].as_str(),
            Some("classifier read budget exhausted before callback")
        );
        assert!(exhausted["cursor"].is_null());
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3]);
        assert_eq!(held_classifier_stages(&store), 0);
        let completed = finish(
            &store,
            store.request(
                &classifier_acquire(&source.path, "budgeted", 1024 * 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(
            store.pin(handle(&completed), &owner).unwrap().event_count,
            5
        );
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3, 3..5]);
    }

    fn parity_classifier(store: &NativeStore, id: &str) -> Arc<Mutex<Vec<Range<usize>>>> {
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

    fn assert_parity_turns(snapshot: &TranscriptSnapshot) {
        let flags: Vec<bool> = (0..snapshot.event_count)
            .map(|position| position % 2 == 0)
            .collect();
        let expected = ActivityIndex::new(&snapshot.entries(), Some(&flags));
        assert_eq!(snapshot.activity.entry_count(), snapshot.event_count);
        assert_eq!(snapshot.activity.turn_count(), expected.turn_count());
        for turn in 0..expected.turn_count() {
            assert_eq!(
                snapshot.activity.turn_bounds(turn),
                expected.turn_bounds(turn)
            );
            assert_eq!(snapshot.activity.prompt(turn), expected.prompt(turn));
        }
    }

    fn classified(
        store: &NativeStore,
        path: &Path,
        id: &str,
        owner: &Value,
    ) -> Arc<TranscriptSnapshot> {
        let response = finish(
            store,
            store.request(
                &classifier_acquire(path, id, 1024 * 1024),
                owner,
                &Cancellation::default(),
            ),
            owner,
        );
        store.pin(handle(&response), owner).unwrap()
    }

    fn users(range: Range<usize>) -> String {
        range
            .map(|index| format!("{}\n", user(&index.to_string())))
            .collect()
    }

    #[test]
    fn appended_source_classifies_only_the_appended_events() {
        let source = Source::new(&users(0..3));
        let store = store();
        let owner = context("a");
        let batches = parity_classifier(&store, "parity");
        let first = classified(&store, &source.path, "parity", &owner);
        assert_eq!(first.event_count, 3);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3]);
        assert_parity_turns(&first);
        let mut expected = vec![0..2, 2..3];
        for round in 1..=4 {
            let start = 1 + 2 * round;
            source.append(&users(start..start + 2));
            let appended = classified(&store, &source.path, "parity", &owner);
            expected.push(start..start + 2);
            assert_eq!(appended.event_count, start + 2);
            assert_eq!(*batches.lock().unwrap(), expected, "round {round}");
            assert_parity_turns(&appended);
        }
        let unchanged = classified(&store, &source.path, "parity", &owner);
        assert_eq!(unchanged.event_count, 11);
        assert_eq!(*batches.lock().unwrap(), expected);
    }

    #[test]
    fn carried_classification_is_dropped_when_the_classifier_or_registry_changes() {
        let source = Source::new(&users(0..3));
        let store = store();
        let owner = context("a");
        let first = parity_classifier(&store, "first");
        let second = parity_classifier(&store, "second");
        classified(&store, &source.path, "first", &owner);
        source.append(&users(3..5));
        classified(&store, &source.path, "second", &owner);
        assert_eq!(*first.lock().unwrap(), vec![0..2, 2..3]);
        assert_eq!(*second.lock().unwrap(), vec![0..2, 2..4, 4..5]);
        let registry = store
            .register_tool_registry(
                &json!([{"name":"read_source","behaves_like":"Read","span_edit":null}]),
                &owner,
            )
            .unwrap();
        let mut reregistered = owner.clone();
        reregistered.insert("registry_generation", json!(registry));
        source.append(&users(5..6));
        classified(&store, &source.path, "first", &reregistered);
        assert_eq!(*first.lock().unwrap(), vec![0..2, 2..3, 0..2, 2..4, 4..6]);
        source.append(&users(6..7));
        classified(&store, &source.path, "first", &owner);
        assert_eq!(
            *first.lock().unwrap(),
            vec![0..2, 2..3, 0..2, 2..4, 4..6, 3..5, 5..7]
        );
    }

    #[test]
    fn rewritten_or_truncated_source_classifies_from_the_start() {
        let source = Source::new(&users(0..3));
        let store = store();
        let owner = context("a");
        let batches = parity_classifier(&store, "parity");
        classified(&store, &source.path, "parity", &owner);
        let contents = std::fs::read_to_string(&source.path).unwrap();
        let tail = contents.rfind("hello").unwrap();
        std::fs::write(
            &source.path,
            format!(
                "{}HELLO{}{}",
                &contents[..tail],
                &contents[tail + "hello".len()..],
                users(3..4)
            ),
        )
        .unwrap();
        let rewritten = classified(&store, &source.path, "parity", &owner);
        assert_eq!(rewritten.event_count, 4);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3, 0..2, 2..4]);
        assert_parity_turns(&rewritten);
        std::fs::write(&source.path, users(0..2)).unwrap();
        let truncated = classified(&store, &source.path, "parity", &owner);
        assert_eq!(truncated.event_count, 2);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3, 0..2, 2..4, 0..2]);
        assert_parity_turns(&truncated);
    }

    #[test]
    fn provisional_tail_labels_are_dropped_and_the_committed_prefix_is_carried() {
        let source = Source::new(users(0..3).trim_end());
        let store = store();
        let owner = context("a");
        let batches = parity_classifier(&store, "parity");
        let provisional = classified(&store, &source.path, "parity", &owner);
        assert!(provisional.provisional_tail);
        assert_eq!(provisional.event_count, 3);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3]);
        source.append(&format!("\n{}", users(3..5)));
        let sealed = classified(&store, &source.path, "parity", &owner);
        assert!(!sealed.provisional_tail);
        assert_eq!(sealed.event_count, 5);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3, 2..4, 4..5]);
        assert_parity_turns(&sealed);
        source.append(users(5..6).trim_end());
        let reopened = classified(&store, &source.path, "parity", &owner);
        assert!(reopened.provisional_tail);
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..3, 2..4, 4..5, 5..6]);
        assert_parity_turns(&reopened);
    }

    #[test]
    fn seeded_classifier_stage_charges_only_the_allocations_it_owns() {
        let source = Source::new(&users(0..20));
        let store = store();
        let owner = context("a");
        let batches = parity_classifier(&store, "parity");
        let previous = classified(&store, &source.path, "parity", &owner);
        source.append(&users(20..22));
        let budget = entry_charges(&previous).into_iter().max().unwrap();
        let mut response = store.request(
            &classifier_acquire(&source.path, "parity", budget),
            &owner,
            &Cancellation::default(),
        );
        while batches.lock().unwrap().last() != Some(&(20..21)) {
            response = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":response["cursor"]}),
                &owner,
                &Cancellation::default(),
            );
        }
        assert_eq!(
            response["reason"].as_str(),
            Some("classifier preparation incomplete"),
            "{response:?}"
        );
        let accounted: usize = store
            .state
            .lock()
            .unwrap()
            .classifier_stages
            .values()
            .map(|slot| slot.accounted.load(Ordering::Acquire))
            .sum();
        assert!(
            accounted < previous.activity.accounted_bytes(),
            "{accounted} >= {}",
            previous.activity.accounted_bytes()
        );
    }

    #[test]
    fn concurrent_appends_never_mix_classified_generations() {
        let source = Arc::new(Source::new(&users(0..4)));
        let store = Arc::new(NativeStore::new(&json!({"max_read_bytes_per_step":4096,"max_events_per_step":3,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":4096,"reserved_hook_leases":1})).unwrap());
        parity_classifier(&store, "parity");
        let rounds = 24;
        let appender = {
            let store = Arc::clone(&store);
            let source = Arc::clone(&source);
            std::thread::spawn(move || {
                let owner = context("appender");
                for round in 0..rounds {
                    let start = 4 + 2 * round;
                    source.append(&users(start..start + 2));
                    let snapshot = classified(&store, &source.path, "parity", &owner);
                    assert_parity_turns(&snapshot);
                }
            })
        };
        let readers: Vec<_> = (0..2)
            .map(|reader| {
                let store = Arc::clone(&store);
                let source = Arc::clone(&source);
                std::thread::spawn(move || {
                    let owner = context(&format!("reader-{reader}"));
                    for _ in 0..rounds {
                        let snapshot = classified(&store, &source.path, "parity", &owner);
                        assert_parity_turns(&snapshot);
                    }
                })
            })
            .collect();
        appender.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        let last = classified(&store, &source.path, "parity", &context("final"));
        assert_eq!(last.event_count, 4 + 2 * rounds);
        assert_parity_turns(&last);
    }

    fn labelled(
        store: &NativeStore,
        path: &Path,
        policy: &Value,
        owner: &Value,
    ) -> (Arc<TranscriptSnapshot>, Vec<Range<usize>>) {
        let mut request = acquire(path);
        request["limits"].insert("max_events", json!(100_000));
        request["limits"].insert("max_items", json!(100_000));
        request["limits"].insert("max_output_bytes", json!(16 * 1024 * 1024));
        request["limits"].insert("max_read_bytes", json!(64 * 1024 * 1024));
        let native = finish(
            store,
            store.request(&request, owner, &Cancellation::default()),
            owner,
        );
        let bounds = limits(&request).unwrap();
        let mut reply = store
            .prepare_classifier(
                handle(&native),
                policy,
                owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        let mut pages = Vec::new();
        while reply["complete"].as_bool() != Some(true) {
            let positions: Vec<usize> = reply["records_json"]
                .as_array()
                .unwrap()
                .iter()
                .map(|record| {
                    sonic_rs::from_str::<Value>(record.as_str().unwrap()).unwrap()["i"]
                        .as_u64()
                        .unwrap() as usize
                })
                .collect();
            pages.push(positions[0]..positions[positions.len() - 1] + 1);
            let labels: Vec<bool> = positions.iter().map(|i| i % 2 == 0).collect();
            reply = store
                .submit_classifier(
                    reply["cursor"].as_str().unwrap(),
                    &labels,
                    owner,
                    &Cancellation::default(),
                )
                .unwrap();
        }
        (
            store.pin(&reply["description"]["handle"], owner).unwrap(),
            pages,
        )
    }

    #[test]
    fn label_pages_resume_after_the_carried_prefix() {
        let source = Source::new(&users(0..3));
        let store = store();
        let owner = context("a");
        let policy = json!({"id":"configured","version":"1"});
        let (first, pages) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(first.event_count, 3);
        assert_eq!(pages, vec![0..3]);
        assert_parity_turns(&first);
        source.append(&users(3..5));
        let (appended, pages) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(appended.event_count, 5);
        assert_eq!(pages, vec![3..5]);
        assert_parity_turns(&appended);
        let (unchanged, pages) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(unchanged.event_count, 5);
        assert!(pages.is_empty());
        assert_parity_turns(&unchanged);
        source.append(&users(5..6));
        let (revised, pages) = labelled(
            &store,
            &source.path,
            &json!({"id":"configured","version":"2"}),
            &owner,
        );
        assert_eq!(revised.event_count, 6);
        assert_eq!(pages, vec![0..6]);
        assert_parity_turns(&revised);
    }

    fn tail_request(path: &Path, count: usize, max_source_read_bytes: usize) -> Value {
        json!({"schema":SCHEMA,"id":"tail","operation":"tail","path":path.to_string_lossy().as_ref(),"count":count,
            "deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_source_read_bytes":max_source_read_bytes,"max_events":1000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
    }

    fn tail_uuids(response: &Value) -> Vec<String> {
        let records: Vec<String> = response["data"]["records_json"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record.as_str().unwrap().to_owned())
            .collect();
        crate::snapshot_codec::decode_events(&records, 1024 * 1024)
            .unwrap()
            .into_iter()
            .map(|record| record.event.meta().unwrap().uuid.clone())
            .collect()
    }

    #[test]
    fn tail_returns_the_newest_events_reading_backwards_in_bounded_steps() {
        let source = Source::new(&users(0..50));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":64,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let response = store.request(
            &tail_request(&source.path, 5, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(tail_uuids(&response), ["45", "46", "47", "48", "49"]);
        assert_eq!(response["data"]["kind"].as_str(), Some("tail"));
        assert_eq!(
            response["data"]["window_start_byte"].as_u64(),
            Some(users(0..45).len() as u64)
        );
        assert_eq!(
            response["data"]["source_bytes"].as_u64(),
            Some(users(0..50).len() as u64)
        );
        let read = response["usage"]["source_bytes_read"].as_u64().unwrap() as usize;
        assert!(
            read > users(45..50).len() && read <= users(44..50).len() + 64,
            "{read}"
        );
        let whole = store.request(
            &tail_request(&source.path, 200, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(whole["status"].as_str(), Some("ok"), "{whole:?}");
        assert_eq!(tail_uuids(&whole).len(), 50);
        assert_eq!(whole["data"]["window_start_byte"].as_u64(), Some(0));
    }

    #[test]
    fn tail_reports_a_bounded_window_when_the_read_budget_stops_the_scan() {
        let source = Source::new(users(0..50).trim_end());
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":64,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let response = store.request(
            &tail_request(&source.path, 5, users(48..50).len()),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(
            response["status"].as_str(),
            Some("incomplete"),
            "{response:?}"
        );
        assert_eq!(
            response["reason"].as_str(),
            Some("tail read budget exhausted")
        );
        assert!(response["cursor"].is_null());
        assert_eq!(tail_uuids(&response), ["48", "49"]);
        assert_eq!(
            response["data"]["window_start_byte"].as_u64(),
            Some(users(0..48).len() as u64)
        );
    }

    #[test]
    fn tail_refuses_codex_sources_and_counts_outside_the_budgets() {
        let codex = Source::new(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n{\"type\":\"event_msg\",\"payload\":{}}\n",
        );
        let store = store();
        let owner = context("a");
        let refused = store.request(
            &tail_request(&codex.path, 1, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(refused["status"].as_str(), Some("invalid_request"));
        assert_eq!(
            refused["reason"].as_str(),
            Some("tail requires a Claude source")
        );
        let claude = Source::new(&users(0..2));
        for count in [0, 257] {
            let refused = store.request(
                &tail_request(&claude.path, count, 1024 * 1024),
                &owner,
                &Cancellation::default(),
            );
            assert_eq!(
                refused["status"].as_str(),
                Some("invalid_request"),
                "{count}"
            );
        }
        let foreign = store.request(
            &tail_request(&claude.path, 1, 1024 * 1024),
            &json!({"claimant":"a","admission":"hook","authority":{"kind":"restricted_roots","effective_uid":unsafe { libc::geteuid() }.to_string(),"roots":[std::env::current_dir().unwrap().to_string_lossy().as_ref()]},"registry_generation":owner["registry_generation"]}),
            &Cancellation::default(),
        );
        assert_eq!(foreign["status"].as_str(), Some("permission_denied"));
    }

    fn windowed_acquire(path: &Path, tail_bytes: u64) -> Value {
        let mut request = acquire(path);
        request.insert("tail_bytes", json!(tail_bytes));
        request
    }

    fn windowed_store() -> NativeStore {
        NativeStore::new(&json!({"max_read_bytes_per_step":1024,"max_events_per_step":64,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
    }

    fn expected_window(path: &Path, tail_bytes: u64) -> (u64, u64, usize) {
        let bytes = std::fs::read(path).unwrap();
        let size = bytes.len() as u64;
        let quantum = tail_bytes / 2;
        let base = if size > tail_bytes {
            (size - tail_bytes) / quantum * quantum
        } else {
            0
        };
        let start = bytes[..base as usize]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at as u64 + 1);
        let first = memchr::memchr_iter(b'\n', &bytes[..start as usize]).count();
        (base, start, first)
    }

    fn windowed(store: &NativeStore, path: &Path, tail_bytes: u64, owner: &Value) -> Value {
        finish(
            store,
            store.request(
                &windowed_acquire(path, tail_bytes),
                owner,
                &Cancellation::default(),
            ),
            owner,
        )
    }

    fn uuids(snapshot: &TranscriptSnapshot) -> Vec<usize> {
        snapshot
            .entries()
            .iter()
            .map(|entry| entry.meta().unwrap().uuid.parse().unwrap())
            .collect()
    }

    #[test]
    fn windowed_acquire_reads_the_window_and_the_entry_cut_at_its_base() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let size = std::fs::metadata(&source.path).unwrap().len();
        let (base, start, first) = expected_window(&source.path, 4096);
        assert!(start < base, "{start} {base}");
        let response = windowed(&store, &source.path, 4096, &owner);
        let description = &response["data"]["description"];
        assert_eq!(description["window_start"].as_u64(), Some(start));
        assert_eq!(description["source_bytes"].as_u64(), Some(size));
        assert_eq!(description["committed_bytes"].as_u64(), Some(size));
        assert_eq!(
            description["event_count"].as_u64(),
            Some((200 - first) as u64)
        );
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        assert_eq!(uuids(&snapshot), (first..200).collect::<Vec<_>>());
        assert_eq!(snapshot.activity.turn_count(), 200 - first);
        let longest = users(0..200).lines().map(str::len).max().unwrap() as u64 + 1;
        assert!(size - start <= 4096 + 2048 + longest);
        let read = store.state.lock().unwrap().counters[1];
        assert!(read >= size - start + 128, "{read}");
        assert!(read <= size - start + 128 + 1024, "{read}");
    }

    #[test]
    fn windowed_acquire_keeps_its_base_across_appends_and_reads_only_the_tail() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let (base, start, first) = expected_window(&source.path, 4096);
        windowed(&store, &source.path, 4096, &owner);
        let before = store.state.lock().unwrap().counters[1];
        let appended = users(200..202);
        source.append(&appended);
        let (same_base, same_start, _) = expected_window(&source.path, 4096);
        assert_eq!((same_base, same_start), (base, start));
        let response = windowed(&store, &source.path, 4096, &owner);
        let description = &response["data"]["description"];
        assert_eq!(description["window_start"].as_u64(), Some(start));
        assert_eq!(
            description["event_count"].as_u64(),
            Some((202 - first) as u64)
        );
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        assert_eq!(uuids(&snapshot), (first..202).collect::<Vec<_>>());
        let read = store.state.lock().unwrap().counters[1] - before;
        assert!(read <= appended.len() as u64 + 192, "{read}");
        assert_eq!(store.state.lock().unwrap().counters[5], 1);
        source.append(&users(202..260));
        let (advanced_base, advanced_start, advanced_first) = expected_window(&source.path, 4096);
        assert!(advanced_base > base);
        let before = store.state.lock().unwrap().counters[1];
        let response = windowed(&store, &source.path, 4096, &owner);
        let description = &response["data"]["description"];
        assert_eq!(description["window_start"].as_u64(), Some(advanced_start));
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        assert_eq!(uuids(&snapshot), (advanced_first..260).collect::<Vec<_>>());
        let size = std::fs::metadata(&source.path).unwrap().len();
        let read = store.state.lock().unwrap().counters[1] - before;
        assert!(read <= size - advanced_start + 128 + 1024, "{read}");
        let state = store.state.lock().unwrap();
        assert_eq!(state.counters[4], 2);
        assert_eq!(state.latest.len(), 2);
    }

    #[test]
    fn whole_file_and_windowed_views_of_one_source_coexist() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let (_, start, first) = expected_window(&source.path, 4096);
        let whole = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let window = windowed(&store, &source.path, 4096, &owner);
        assert_eq!(
            whole["data"]["description"]["window_start"].as_u64(),
            Some(0)
        );
        assert_eq!(
            whole["data"]["description"]["event_count"].as_u64(),
            Some(200)
        );
        assert_eq!(
            window["data"]["description"]["window_start"].as_u64(),
            Some(start)
        );
        assert_eq!(
            window["data"]["description"]["event_count"].as_u64(),
            Some((200 - first) as u64)
        );
        assert_ne!(
            handle(&whole)["snapshot_id"].as_str(),
            handle(&window)["snapshot_id"].as_str()
        );
        let before = store.state.lock().unwrap().counters[1];
        let whole_again = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let window_again = windowed(&store, &source.path, 4096, &owner);
        assert_eq!(
            handle(&whole_again)["snapshot_id"].as_str(),
            handle(&whole)["snapshot_id"].as_str()
        );
        assert_eq!(
            handle(&window_again)["snapshot_id"].as_str(),
            handle(&window)["snapshot_id"].as_str()
        );
        {
            let state = store.state.lock().unwrap();
            assert_eq!(state.counters[1], before);
            assert_eq!(state.counters[7], 2);
            assert_eq!(state.latest.len(), 2);
        }
        let covering = finish(
            &store,
            store.request(
                &windowed_acquire(&source.path, 1024 * 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(
            handle(&covering)["snapshot_id"].as_str(),
            handle(&whole)["snapshot_id"].as_str()
        );
    }

    #[test]
    fn windowed_classifier_labels_only_the_appended_events() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let batches = parity_classifier(&store, "parity");
        let (_, _, first) = expected_window(&source.path, 4096);
        let mut request = classifier_acquire(&source.path, "parity", 1024 * 1024);
        request.insert("tail_bytes", json!(4096));
        let initial = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        let snapshot = store.pin(handle(&initial), &owner).unwrap();
        assert_eq!(snapshot.event_count, 200 - first);
        assert_parity_turns(&snapshot);
        let labelled = batches.lock().unwrap().clone();
        assert_eq!(labelled.first().unwrap().start, 0);
        assert_eq!(labelled.last().unwrap().end, 200 - first);
        source.append(&users(200..202));
        let appended = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        let snapshot = store.pin(handle(&appended), &owner).unwrap();
        assert_eq!(uuids(&snapshot), (first..202).collect::<Vec<_>>());
        assert_parity_turns(&snapshot);
        assert_eq!(
            batches.lock().unwrap()[labelled.len()..],
            [200 - first..202 - first]
        );
    }

    #[test]
    fn windowed_acquire_refuses_codex_sources_and_zero_windows() {
        let codex = Source::new(&format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"s\"}}}}\n{}",
            (0..64)
                .map(|_| "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"m\"}}\n")
                .collect::<String>()
        ));
        let store = windowed_store();
        let owner = context("a");
        let refused = finish(
            &store,
            store.request(
                &windowed_acquire(&codex.path, 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(refused["status"].as_str(), Some("invalid_request"));
        assert_eq!(
            refused["reason"].as_str(),
            Some("tail_bytes requires a Claude source")
        );
        let claude = Source::new(&users(0..2));
        let zero = store.request(
            &windowed_acquire(&claude.path, 0),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(zero["status"].as_str(), Some("invalid_request"));
        let size = std::fs::metadata(&codex.path).unwrap().len();
        for tail_bytes in [size, size - 1, 1024 * 1024] {
            let covered = finish(
                &store,
                store.request(
                    &windowed_acquire(&codex.path, tail_bytes),
                    &owner,
                    &Cancellation::default(),
                ),
                &owner,
            );
            assert_eq!(
                covered["reason"].as_str(),
                Some("tail_bytes requires a Claude source"),
                "{tail_bytes}: {covered:?}"
            );
        }
        let whole = finish(
            &store,
            store.request(&acquire(&codex.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(whole["status"].as_str(), Some("ok"), "{whole:?}");
        let cached = finish(
            &store,
            store.request(
                &windowed_acquire(&codex.path, 1024 * 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(cached["status"].as_str(), Some("invalid_request"));
        assert_eq!(
            cached["reason"].as_str(),
            Some("tail_bytes requires a Claude source")
        );
    }

    #[test]
    fn windowed_root_attached_to_itself_is_not_its_own_child() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let root = windowed(&store, &source.path, 4096, &owner);
        let (_, _, first) = expected_window(&source.path, 4096);
        assert!(first > 0);
        let records = graph_records(
            &store,
            &graph_request(
                &root,
                json!({"kind":"sidechain_membership","order":"forward"}),
                vec![source.path.to_string_lossy().into_owned()],
                json!([]),
            ),
            &owner,
        );
        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn windowed_scan_locates_the_entry_before_the_entry_bound_applies() {
        let source = Source::new(&users(0..200));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":4096,"max_events_per_step":64,"max_entry_bytes":1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let (_, start, first) = expected_window(&source.path, 4096);
        let response = windowed(&store, &source.path, 4096, &owner);
        let description = &response["data"]["description"];
        assert_eq!(description["window_start"].as_u64(), Some(start));
        assert_eq!(
            description["event_count"].as_u64(),
            Some((200 - first) as u64)
        );
        let unbroken = Source::new(&format!(
            "{}{}\n{}",
            users(0..5),
            "x".repeat(8192),
            users(5..10)
        ));
        let refused = finish(
            &store,
            store.request(
                &windowed_acquire(&unbroken.path, 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(
            refused["status"].as_str(),
            Some("entry_limit"),
            "{refused:?}"
        );
    }

    #[test]
    fn null_tail_bytes_is_a_whole_file_acquire() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let whole = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut request = acquire(&source.path);
        request.insert("tail_bytes", json!(null));
        let unwindowed = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            handle(&unwindowed)["snapshot_id"].as_str(),
            handle(&whole)["snapshot_id"].as_str()
        );
        assert_eq!(
            unwindowed["data"]["description"]["window_start"].as_u64(),
            Some(0)
        );
    }

    #[test]
    fn windowed_root_graph_excludes_its_own_file_from_direct_paths() {
        let source = Source::new(&users(0..200));
        let store = windowed_store();
        let owner = context("a");
        let root = windowed(&store, &source.path, 4096, &owner);
        let template = acquire(&source.path);
        let before = store.state.lock().unwrap().counters[1];
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[source.path.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let state = store.state.lock().unwrap();
        let prepared = state
            .prepared_graphs
            .values()
            .next()
            .unwrap()
            .lock()
            .unwrap();
        assert!(prepared.sources.is_empty());
        assert_eq!(prepared.stamps.len(), 1);
        assert_eq!(state.counters[1], before);
    }

    #[test]
    fn concurrent_appends_never_mix_windowed_generations() {
        let source = Arc::new(Source::new(&users(0..40)));
        let store = Arc::new(NativeStore::new(&json!({"max_read_bytes_per_step":1024,"max_events_per_step":3,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":4096,"reserved_hook_leases":1})).unwrap());
        parity_classifier(&store, "parity");
        let rounds = 24;
        let view = |store: &NativeStore, path: &Path, owner: &Value| {
            let mut request = classifier_acquire(path, "parity", 1024 * 1024);
            request.insert("tail_bytes", json!(2048));
            let response = finish(
                store,
                store.request(&request, owner, &Cancellation::default()),
                owner,
            );
            let snapshot = store.pin(handle(&response), owner).unwrap();
            let ids = uuids(&snapshot);
            assert_eq!(
                ids,
                (ids[0]..ids[0] + ids.len()).collect::<Vec<_>>(),
                "{ids:?}"
            );
            assert!(snapshot.stamp.size - snapshot.window_start <= 2048 + 1024 + 256);
            assert_parity_turns(&snapshot);
            snapshot
        };
        let appender = {
            let store = Arc::clone(&store);
            let source = Arc::clone(&source);
            std::thread::spawn(move || {
                let owner = context("appender");
                for round in 0..rounds {
                    let start = 40 + 2 * round;
                    source.append(&users(start..start + 2));
                    view(&store, &source.path, &owner);
                }
            })
        };
        let readers: Vec<_> = (0..2)
            .map(|reader| {
                let store = Arc::clone(&store);
                let source = Arc::clone(&source);
                std::thread::spawn(move || {
                    let owner = context(&format!("reader-{reader}"));
                    for _ in 0..rounds {
                        view(&store, &source.path, &owner);
                    }
                })
            })
            .collect();
        appender.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        let last = view(&store, &source.path, &context("final"));
        assert_eq!(*uuids(&last).last().unwrap(), 40 + 2 * rounds - 1);
    }

    #[test]
    fn carried_labels_publish_within_the_page_reservation() {
        let source = Source::new(&users(0..5000));
        let store = NativeStore::new(&json!({"max_events_per_step":4096,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":64,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let policy = json!({"id":"configured","version":"1"});
        let (first, _) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(first.event_count, 5000);
        let (again, pages) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(again.event_count, 5000);
        assert!(pages.is_empty());
        source.append(&users(5000..5001));
        let (appended, pages) = labelled(&store, &source.path, &policy, &owner);
        assert_eq!(appended.event_count, 5001);
        assert_eq!(pages, vec![5000..5001]);
        assert_parity_turns(&appended);
    }

    #[test]
    fn tail_clips_to_the_output_budget_the_reply_is_graded_against() {
        let source = Source::new(
            &(0..4)
                .map(|index| {
                    format!(
                        "{}\n",
                        user(&index.to_string()).replace("hello", &"\\\"x".repeat(1024))
                    )
                })
                .collect::<String>(),
        );
        let store = store();
        let owner = context("a");
        let mut request = tail_request(&source.path, 4, 1024 * 1024);
        request["limits"].insert("max_output_bytes", json!(7 * 1024));
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(
            response["status"].as_str(),
            Some("incomplete"),
            "{response:?}"
        );
        assert_eq!(
            response["reason"].as_str(),
            Some("tail output budget exhausted")
        );
        let uuids = tail_uuids(&response);
        assert!(!uuids.is_empty() && uuids.len() < 4, "{uuids:?}");
        assert_eq!(uuids.last().map(String::as_str), Some("3"));
    }

    #[test]
    fn tail_does_not_take_a_torn_trailing_line_as_proof_of_a_claude_source() {
        let torn = Source::new(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n{\"type\":\"user\",\"uuid\":\"t",
        );
        let store = store();
        let owner = context("a");
        let refused = store.request(
            &tail_request(&torn.path, 1, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(
            refused["status"].as_str(),
            Some("invalid_request"),
            "{refused:?}"
        );
        assert_eq!(
            refused["reason"].as_str(),
            Some("tail requires a Claude source")
        );
        let claude = Source::new(&format!("{}{{\"type\":\"user\",\"uuid\":\"t", users(0..2)));
        let skipped = store.request(
            &tail_request(&claude.path, 5, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(skipped["status"].as_str(), Some("ok"), "{skipped:?}");
        assert_eq!(tail_uuids(&skipped), ["0", "1"]);
    }

    #[test]
    fn classifier_event_larger_than_the_whole_budget_fails_incomplete() {
        let large = format!(
            r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#,
            "x".repeat(64 * 1024)
        );
        let source = Source::new(&format!("{}\n{large}\n{}\n", user("a"), user("c")));
        let store = NativeStore::new(
            &json!({"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096}),
        )
        .unwrap();
        let owner = context("a");
        let native = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let charges = entry_charges(&store.pin(handle(&native), &owner).unwrap());
        let batches = recording_classifier(&store, "oversized");
        let budget = charges[1] - 1;
        assert!(charges[0] + charges[2] < budget);
        let first = store.request(
            &classifier_acquire(&source.path, "oversized", budget),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        let failed = store.request(
            &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":first["cursor"]}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(failed["status"].as_str(), Some("incomplete"));
        assert!(failed["cursor"].is_null());
        let retried = store.request(
            &classifier_acquire(&source.path, "oversized", budget),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(retried["status"].as_str(), Some("incomplete"));
        assert_eq!(
            retried["reason"].as_str(),
            Some("classifier read budget exhausted before callback")
        );
        assert!(retried["cursor"].is_null());
        assert_eq!(*batches.lock().unwrap(), vec![0..1]);
        let state = store.state.lock().unwrap();
        assert!(state.waiters.is_empty());
        assert!(state
            .classifier_stages
            .values()
            .all(|slot| Arc::strong_count(slot) == 1));
    }

    #[test]
    fn failed_classifier_stage_frees_admission_immediately() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = NativeStore::new(&json!({"max_pending_loads":1,"reserved_hook_loads":0,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
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
        let batches = recording_classifier(&store, "working");
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
        assert_eq!(held_classifier_stages(&store), 0);
        let completed = finish(
            &store,
            store.request(
                &classifier_acquire(&source.path, "working", 1024 * 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(
            store.pin(handle(&completed), &owner).unwrap().event_count,
            2
        );
        assert_eq!(*batches.lock().unwrap(), vec![0..2]);
    }

    #[test]
    fn parked_classifier_stage_holds_admission_until_its_reservation_is_released() {
        let source = Source::new(&format!("{}\n{}\n{}\n", user("a"), user("b"), user("c")));
        let store = NativeStore::new(&json!({"max_pending_loads":1,"reserved_hook_loads":0,"max_events_per_step":1,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        recording_classifier(&store, "parked");
        let waiting = recording_classifier(&store, "waiting");
        let parked = store.request(
            &classifier_acquire(&source.path, "parked", 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(parked["status"].as_str(), Some("incomplete"), "{parked:?}");
        assert_eq!(held_classifier_stages(&store), 1);
        let contended = store.request(
            &classifier_acquire(&source.path, "waiting", 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(contended["status"].as_str(), Some("retained_limit"));
        assert_eq!(
            contended["reason"].as_str(),
            Some("classifier preparation admission exhausted")
        );
        let released = store.request(
            &json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"cursor","token":parked["cursor"]}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
        assert_eq!(held_classifier_stages(&store), 0);
        let completed = finish(
            &store,
            store.request(
                &classifier_acquire(&source.path, "waiting", 1024 * 1024),
                &owner,
                &Cancellation::default(),
            ),
            &owner,
        );
        assert_eq!(
            store.pin(handle(&completed), &owner).unwrap().event_count,
            3
        );
        assert_eq!(*waiting.lock().unwrap(), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn classifier_facts_charge_only_user_text_among_large_early_tool_payloads() {
        let payload = "x".repeat(700 * 1024);
        let tool_use = json!({"type":"assistant","uuid":"write","sessionId":"s","timestamp":"2026-01-02T03:04:06Z",
            "message":{"model":"m","content":[{"type":"tool_use","id":"w","name":"Write","input":{"file_path":"a.rs","content":payload}}]}});
        let tool_result = json!({"type":"user","uuid":"result","sessionId":"s","timestamp":"2026-01-02T03:04:07Z",
            "message":{"content":[{"type":"tool_result","tool_use_id":"w","content":payload}]}});
        let instruction = json!({"type":"user","uuid":"instruction","sessionId":"s","timestamp":"2026-01-02T03:04:08Z",
            "message":{"content":"<system_instruction> lane"}});
        let source = Source::new(&format!(
            "{}\n{}\n{}\n{}\n",
            user("a"),
            sonic_rs::to_string(&tool_use).unwrap(),
            sonic_rs::to_string(&tool_result).unwrap(),
            sonic_rs::to_string(&instruction).unwrap()
        ));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_entry_bytes":4*1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let mut request = acquire(&source.path);
        request["limits"].insert("max_source_read_bytes", json!(16 * 1024 * 1024));
        let snapshot = store
            .pin(
                handle(&finish(
                    &store,
                    store.request(&request, &owner, &Cancellation::default()),
                    &owner,
                )),
                &owner,
            )
            .unwrap();
        let limits = WorkLimits {
            max_read_bytes: 1024 * 1024,
            max_source_read_bytes: 1024 * 1024,
            max_events: 1000,
            max_items: 256,
            max_output_bytes: 1024 * 1024,
            max_discovery_entries: 1000,
            max_sources: 100,
            deadline_unix_ms: now_ms() + 30_000,
        };
        assert!(entry_charges(&snapshot)[..3].iter().sum::<usize>() > limits.max_read_bytes);
        let mut usage = crate::snapshot_projection::ProjectionUsage::default();
        let facts = crate::snapshot_projection::classifier_facts(
            &snapshot,
            "<system_instruction>",
            50,
            &limits,
            &Cancellation::default(),
            &mut usage,
        )
        .unwrap();
        assert_eq!(
            facts,
            json!({"has_users":true,"all_users_sidechain":false,"has_user_prefix":true})
        );
        assert_eq!(usage.events, 4);
        assert!(usage.read_bytes < 64 * 1024, "{}", usage.read_bytes);
    }

    #[test]
    fn source_reads_charge_only_the_source_budget() {
        let source = Source::new(
            &(0..8)
                .map(|index| format!("{}\n", user(&index.to_string())))
                .collect::<String>(),
        );
        let size = std::fs::metadata(&source.path).unwrap().len() as usize;
        let store = store();
        let owner = context("a");
        let mut request = acquire(&source.path);
        request["limits"].insert("max_source_read_bytes", json!(size / 2));
        request["limits"].insert("max_read_bytes", json!(64 * 1024 * 1024));
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        let remaining = &first["data"]["reservation"]["remaining_work"];
        assert_eq!(
            remaining["max_source_read_bytes"].as_u64(),
            Some((size / 2) as u64 - first["usage"]["source_bytes_read"].as_u64().unwrap())
        );
        assert_eq!(remaining["max_read_bytes"].as_u64(), Some(64 * 1024 * 1024));
        let exhausted = finish_prepared(&store, first, &owner);
        assert_eq!(exhausted["status"].as_str(), Some("incomplete"));
        assert_eq!(exhausted["reason"].as_str(), Some("source_read_limit"));
        assert!(exhausted["cursor"].is_null());
        assert!(store.state.lock().unwrap().waiters.is_empty());
    }

    #[test]
    fn a_waiter_starved_at_the_seal_does_not_fail_the_shared_load() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let size = std::fs::metadata(&source.path).unwrap().len();
        let store = store();
        let (patient, starved) = (context("patient"), context("starved"));
        let waiting = store.request(&acquire(&source.path), &patient, &Cancellation::default());
        assert_eq!(
            waiting["status"].as_str(),
            Some("incomplete"),
            "{waiting:?}"
        );
        let mut request = acquire(&source.path);
        request["limits"].insert("max_source_read_bytes", json!(size));
        let exhausted = finish_prepared(
            &store,
            store.request(&request, &starved, &Cancellation::default()),
            &starved,
        );
        assert_eq!(
            exhausted["reason"].as_str(),
            Some("source_read_limit"),
            "{exhausted:?}"
        );
        let completed = finish(&store, waiting, &patient);
        assert_eq!(completed["status"].as_str(), Some("ok"), "{completed:?}");
    }

    #[test]
    fn classifier_charges_do_not_spend_the_source_budget() {
        let source = Source::new(
            &(0..5)
                .map(|index| format!("{}\n", user(&index.to_string())))
                .collect::<String>(),
        );
        let size = std::fs::metadata(&source.path).unwrap().len() as usize;
        let probe = store();
        let owner = context("a");
        let native = finish(
            &probe,
            probe.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let charge: usize = entry_charges(&probe.pin(handle(&native), &owner).unwrap())
            .iter()
            .sum();
        let store = store();
        let batches = recording_classifier(&store, "cold");
        let mut request = classifier_acquire(&source.path, "cold", charge);
        request["limits"].insert("max_source_read_bytes", json!(size + 128));
        let completed = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(completed["status"].as_str(), Some("ok"), "{completed:?}");
        assert_eq!(
            store.pin(handle(&completed), &owner).unwrap().event_count,
            5
        );
        assert_eq!(*batches.lock().unwrap(), vec![0..2, 2..4, 4..5]);
    }

    #[test]
    fn acquire_joins_a_shared_load_after_the_source_appends() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let a = context("a");
        let b = context("b");
        let pinned_size = std::fs::metadata(&source.path).unwrap().len();
        let first = store.request(&acquire(&source.path), &a, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        source.append(&format!("{}\n", user("c")));
        let second = store.request(&acquire(&source.path), &b, &Cancellation::default());
        assert_eq!(second["status"].as_str(), Some("incomplete"), "{second:?}");
        assert_eq!(
            first["data"]["reservation"]["load_id"].as_str(),
            second["data"]["reservation"]["load_id"].as_str()
        );
        for (response, owner) in [(first, &a), (second, &b)] {
            let snapshot = store
                .pin(handle(&finish(&store, response, owner)), owner)
                .unwrap();
            assert_eq!(snapshot.stamp.size, pinned_size);
            assert_eq!(snapshot.event_count, 2);
        }
    }

    #[test]
    fn prepared_graph_survives_root_appends_and_rejects_root_rewrites() {
        let source = Source::new(&format!("{}\n", user("root")));
        let child = source.directory.join("child.jsonl");
        std::fs::write(&child, format!("{}\n", user("child"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[child.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = || {
            finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            )
        };
        source.append(&format!("{}\n", user("appended")));
        let mut starved = template["limits"].clone();
        starved.insert("max_source_read_bytes", json!(1));
        let starved = store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":starved}), &owner, &Cancellation::default());
        assert_eq!(
            starved["status"].as_str(),
            Some("incomplete"),
            "{starved:?}"
        );
        assert_eq!(starved["reason"].as_str(), Some("source_read_limit"));
        let appended = query();
        assert_eq!(appended["status"].as_str(), Some("ok"), "{appended:?}");
        assert_eq!(appended["data"]["value"].as_bool(), Some(false));
        source.append(&format!("{}\n", user("again")));
        assert_eq!(query()["status"].as_str(), Some("ok"));
        let contents = std::fs::read_to_string(&source.path).unwrap();
        std::fs::write(&source.path, contents.replacen("hello", "HELLO", 1)).unwrap();
        let rewritten = query();
        assert_eq!(
            rewritten["status"].as_str(),
            Some("changed"),
            "{rewritten:?}"
        );
        assert_eq!(
            rewritten["reason"].as_str(),
            Some("prepared root generation changed")
        );
        std::fs::write(&source.path, "").unwrap();
        assert_eq!(query()["status"].as_str(), Some("changed"));
    }

    #[test]
    fn prepared_graph_reuses_one_source_across_queries_and_events() {
        let source = Source::new(&format!("{}\n", user("root")));
        let attachment = source.directory.join("external.jsonl");
        std::fs::write(&attachment, format!("{}\n", user("external"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let direct_paths = vec![attachment.to_string_lossy().into_owned(); 923];
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":direct_paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let before_prepare = store.state.lock().unwrap().counters;
        let prepared = finish(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
        let graph = prepared["data"]["handle"].clone();
        assert_eq!(
            store.state.lock().unwrap().prepared_graphs[graph["graph_id"].as_str().unwrap()]
                .lock()
                .unwrap()
                .sources
                .len(),
            1
        );
        let after_prepare = store.state.lock().unwrap().counters;
        assert_eq!(after_prepare[1], before_prepare[1]);
        let mut after_first = after_prepare;
        for query in [
            json!({"kind":"has_tool","pattern":"Read","subagents":true}),
            json!({"kind":"has_read","pattern":"missing","subagents":true}),
        ] {
            let response = finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph,"selectors":[],"query":query,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(response["data"]["value"].as_bool(), Some(false));
            if after_first[1] == after_prepare[1] {
                after_first = store.state.lock().unwrap().counters;
            }
        }
        let after_queries = store.state.lock().unwrap().counters;
        assert!(after_first[1] > after_prepare[1]);
        assert_eq!(after_queries[1], after_first[1]);
        assert_eq!(after_queries[15], after_first[15]);
        let prepared_again = finish(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            prepared_again["status"].as_str(),
            Some("ok"),
            "{prepared_again:?}"
        );
        let after_again = store.state.lock().unwrap().counters;
        assert_eq!(after_again[1], after_first[1]);
        assert_eq!(after_again[15], after_first[15]);
        assert_eq!(store.prepared_disk.stats().writes, 2);
        let state = store.state.lock().unwrap();
        let first_id = graph["graph_id"].as_str().unwrap();
        let second_id = prepared_again["data"]["handle"]["graph_id"]
            .as_str()
            .unwrap();
        let first = state.prepared_graphs[first_id].lock().unwrap();
        let second = state.prepared_graphs[second_id].lock().unwrap();
        assert!(Arc::ptr_eq(&first.root_facts, &second.root_facts));
    }

    #[test]
    fn root_positive_query_does_not_open_registered_sources() {
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/root"}}]}}"#;
        let source = Source::new(&format!("{}\n{tool}\n", user("root")));
        let ids: Vec<_> = (0..929).map(|index| format!("thread-{index:04}")).collect();
        for (index, id) in ids.iter().enumerate() {
            let file = File::create(source.directory.join(format!("{id}.jsonl"))).unwrap();
            file.set_len(if index == 0 { 39_922_816 } else { 889_173 })
                .unwrap();
        }
        let store = NativeStore::new(&json!({
            "max_events_per_step":2048,
            "max_retained_bytes":32*1024*1024,
            "reserved_hook_accounted_bytes":4096,
            "max_leases":16,
            "reserved_hook_leases":1
        }))
        .unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut template = acquire(&source.path);
        template["limits"].insert("max_source_read_bytes", json!(1024));
        template["limits"].insert("max_events", json!(1024));
        template["limits"].insert("max_items", json!(1024));
        template["limits"].insert("max_discovery_entries", json!(2048));
        template["limits"].insert("max_sources", json!(1024));
        let mut background = owner.clone();
        background.insert("work_class", json!("background"));
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let cold = store.request(&prepare, &owner, &Cancellation::default());
        assert_eq!(cold["status"].as_str(), Some("incomplete"), "{cold:?}");
        assert_eq!(
            cold["usage"]["discovery_entries_examined"].as_u64(),
            Some(0)
        );
        let warm = store.request(&json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":prepare["thread_ids"],"roots":prepare["roots"],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &background, &Cancellation::default());
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm["data"]["complete"].as_bool(), Some(false));
        assert!(
            warm["usage"]["discovery_entries_examined"]
                .as_u64()
                .unwrap()
                > 0
        );
        let after_warm = store.state.lock().unwrap().counters;
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert_eq!(
            graph["usage"]["discovery_entries_examined"].as_u64(),
            Some(0)
        );
        let result = store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
        assert_eq!(result["data"]["value"].as_bool(), Some(true));
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[0], after_warm[0]);
        assert_eq!(after[1], after_warm[1]);
        assert_eq!(store.prepared_disk.stats().writes, 1);
    }

    #[test]
    fn prepared_graph_rejects_changed_source_and_reuses_other_revisions() {
        let source = Source::new(&format!("{}\n", user("root")));
        let changed = source.directory.join("changed.jsonl");
        let stable = source.directory.join("stable.jsonl");
        std::fs::write(&changed, format!("{}\n", user("before"))).unwrap();
        std::fs::write(&stable, format!("{}\n", user("stable"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[changed.to_string_lossy().as_ref(),stable.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = finish(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let old_handle = first["data"]["handle"].clone();
        let query = |graph: &Value| {
            finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph,"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            )
        };
        let warmed = query(&old_handle);
        assert_eq!(warmed["status"].as_str(), Some("ok"), "{warmed:?}");
        let before = store.state.lock().unwrap().counters;
        std::fs::write(&changed, format!("{}\n", user("after-longer"))).unwrap();
        let stale = query(&old_handle);
        assert_eq!(stale["status"].as_str(), Some("changed"), "{stale:?}");
        let second = finish(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        let fresh = query(&second["data"]["handle"]);
        assert_eq!(fresh["status"].as_str(), Some("ok"), "{fresh:?}");
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[0] - before[0], 1);
        assert!(after[1] > before[1]);
    }

    #[test]
    fn prepared_negative_rechecks_sidechain_membership() {
        let source = Source::new(&format!("{}\n", user("root")));
        let child = source.directory.join("child.jsonl");
        std::fs::write(&child, format!("{}\n", user("child"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[child.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let subagents = source.directory.join("child/subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-new.jsonl"),
            format!("{}\n", user("new")),
        )
        .unwrap();
        let query = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(query["status"].as_str(), Some("changed"), "{query:?}");
    }

    #[test]
    fn cached_unregistered_source_cannot_answer_another_graph() {
        let source = Source::new(&format!("{}\n", user("root")));
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/only-a"}}]}}"#;
        let a = source.directory.join("a.jsonl");
        let b = source.directory.join("b.jsonl");
        std::fs::write(&a, format!("{}\n{tool}\n", user("a"))).unwrap();
        std::fs::write(&b, format!("{}\n", user("b"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let prepare = |path: &Path| {
            finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[path.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            )
        };
        let query = |graph: &Value| {
            finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph,"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            )
        };
        let graph_a = prepare(&a);
        assert_eq!(graph_a["status"].as_str(), Some("ok"), "{graph_a:?}");
        let found = query(&graph_a["data"]["handle"]);
        assert_eq!(found["data"]["value"].as_bool(), Some(true));
        let graph_b = prepare(&b);
        assert_eq!(graph_b["status"].as_str(), Some("ok"), "{graph_b:?}");
        let absent = query(&graph_b["data"]["handle"]);
        assert_eq!(absent["status"].as_str(), Some("ok"), "{absent:?}");
        assert_eq!(absent["data"]["value"].as_bool(), Some(false));
    }

    #[test]
    fn prepared_input_pages_keep_order_without_more_source_reads() {
        let source = Source::new(&format!("{}\n", user("root")));
        let mut paths = Vec::new();
        for index in 0..258 {
            let path = source.directory.join(format!("external-{index:03}.jsonl"));
            let tool = format!(
                r#"{{"type":"assistant","uuid":"tool-{index}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"bash-{index}","name":"Bash","input":{{"command":"echo {index}"}}}}]}}}}"#
            );
            std::fs::write(&path, format!("{}\n{tool}\n", user(&format!("u-{index}")))).unwrap();
            paths.push(path.to_string_lossy().into_owned());
        }
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut template = acquire(&source.path);
        template["limits"].insert("max_sources", json!(512));
        template["limits"].insert("max_items", json!(1024));
        template["limits"].insert("max_events", json!(4096));
        let prepare = finish_prepared(&store, store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner);
        assert_eq!(prepare["status"].as_str(), Some("ok"), "{prepare:?}");
        let mut page = store.request(&json!({"schema":SCHEMA,"id":"inputs","operation":"query_graph","handle":prepare["data"]["handle"],"selectors":[],"query":{"kind":"deep_predicate_inputs","order":"forward"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        let mut records = Vec::new();
        for _ in 0..512 {
            records.extend(
                page["data"]["records_json"]
                    .as_array()
                    .expect("records")
                    .iter()
                    .map(|item| item.as_str().unwrap().to_owned()),
            );
            if page["status"].as_str() == Some("ok") {
                break;
            }
            assert_eq!(page["status"].as_str(), Some("incomplete"), "{page:?}");
            page = store.request(&json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":page["cursor"]}), &owner, &Cancellation::default());
        }
        assert_eq!(page["status"].as_str(), Some("ok"), "{page:?}");
        assert_eq!(records.len(), 259);
        assert!(records[1].contains("echo 0"), "{}", records[1]);
        assert!(records[258].contains("echo 257"));
        let after_first = store.state.lock().unwrap().counters;
        let repeated = store.request(&json!({"schema":SCHEMA,"id":"repeat","operation":"query_graph","handle":prepare["data"]["handle"],"selectors":[],"query":{"kind":"deep_predicate_inputs","order":"forward"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(repeated["status"].as_str(), Some("incomplete"));
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let abandoned = store.request(&json!({"schema":SCHEMA,"id":"cancel","operation":"resume","cursor":repeated["cursor"]}), &owner, &cancelled);
        assert_eq!(abandoned["status"].as_str(), Some("cancelled"));
        let still_usable = finish_prepared(&store, store.request(&json!({"schema":SCHEMA,"id":"other","operation":"query_graph","handle":prepare["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner);
        assert_eq!(
            still_usable["status"].as_str(),
            Some("ok"),
            "{still_usable:?}"
        );
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[1], after_first[1]);
        assert_eq!(after[15], after_first[15]);
    }

    #[test]
    fn deep_predicate_inputs_split_a_window_over_the_record_bound() {
        let commands: Vec<String> = (0..48)
            .map(|index| format!("echo {index:02} {}", "x".repeat(32 * 1024)))
            .collect();
        let tools: String = commands
            .iter()
            .enumerate()
            .map(|(index, command)| format!(
                "{}\n",
                format_args!(r#"{{"type":"assistant","uuid":"tool-{index}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"bash-{index}","name":"Bash","input":{{"command":"{command}"}}}}]}}}}"#)
            ))
            .collect();
        let source = Source::new(&format!("{}\n{tools}", user("root")));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":4*1024*1024,"max_events_per_step":256,"max_entry_bytes":64*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let mut template = acquire(&source.path);
        template["limits"].insert("max_read_bytes", json!(64 * 1024 * 1024));
        template["limits"].insert("max_source_read_bytes", json!(64 * 1024 * 1024));
        template["limits"].insert("max_output_bytes", json!(16 * 1024 * 1024));
        let root = finish(
            &store,
            store.request(&template, &owner, &Cancellation::default()),
            &owner,
        );
        let commands_of = |records: &[String]| -> Vec<String> {
            assert!(records.len() > 1, "{} records", records.len());
            for record in records {
                assert!(record.len() <= crate::snapshot_codec::MAX_RECORD_BYTES);
            }
            crate::snapshot_codec::decode_predicate_inputs(
                records,
                crate::snapshot_codec::MAX_PAGE_BYTES,
            )
            .unwrap()
            .into_iter()
            .flat_map(|record| record.commands)
            .collect()
        };
        let pages = |mut page: Value| -> Vec<String> {
            let mut records = Vec::new();
            for _ in 0..64 {
                records.extend(
                    page["data"]["records_json"]
                        .as_array()
                        .unwrap_or_else(|| panic!("{page:?}"))
                        .iter()
                        .map(|item| item.as_str().unwrap().to_owned()),
                );
                if page["status"].as_str() == Some("ok") {
                    return records;
                }
                assert_eq!(page["status"].as_str(), Some("incomplete"), "{page:?}");
                page = store.request(&json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":page["cursor"]}), &owner, &Cancellation::default());
            }
            panic!("deep predicate inputs did not finish");
        };
        let mut query = graph_request(
            &root,
            json!({"kind":"deep_predicate_inputs","order":"forward"}),
            Vec::new(),
            json!([]),
        );
        query["limits"] = template["limits"].clone();
        let lease = pages(store.request(&query, &owner, &Cancellation::default()));
        assert_eq!(commands_of(&lease), commands);
        let prepare = finish_prepared(&store, store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner);
        assert_eq!(prepare["status"].as_str(), Some("ok"), "{prepare:?}");
        let prepared = pages(store.request(&json!({"schema":SCHEMA,"id":"inputs","operation":"query_graph","handle":prepare["data"]["handle"],"selectors":[],"query":{"kind":"deep_predicate_inputs","order":"forward"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()));
        assert_eq!(commands_of(&prepared), commands);
    }

    #[test]
    fn expired_prepared_query_reports_deadline_after_pruning() {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths: Vec<_> = (0..9)
            .map(|index| {
                let path = source.directory.join(format!("external-{index}.jsonl"));
                std::fs::write(&path, format!("{}\n", user(&format!("u-{index}")))).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let page = store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(page["status"].as_str(), Some("incomplete"), "{page:?}");
        let token = page["cursor"].as_str().unwrap();
        {
            let mut state = store.state.lock().unwrap();
            let mut cursor = state.prepared_queries.get_mut(token).unwrap();
            cursor.expires = now_ms() - 1;
            cursor.remaining.deadline_unix_ms = now_ms() - 1;
        }
        let resumed = store.request(
            &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":token}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(resumed["status"].as_str(), Some("deadline"), "{resumed:?}");
    }

    #[test]
    fn bounded_queries_reuse_finished_sources_until_complete() {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths: Vec<_> = (0..6)
            .map(|index| {
                let path = source.directory.join(format!("external-{index}.jsonl"));
                std::fs::write(&path, format!("{}\n", user(&format!("u-{index}")))).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let baseline = store.state.lock().unwrap().counters;
        let mut template = acquire(&source.path);
        template["limits"].insert("max_source_read_bytes", json!(1024));
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let request = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let mut prior = 0;
        let mut complete = false;
        for _ in 0..8 {
            let mut result = store.request(&request, &owner, &Cancellation::default());
            while let Some(cursor) = result["cursor"].as_str() {
                result = store.request(
                    &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                    &owner,
                    &Cancellation::default(),
                );
            }
            let cached = store.prepared_disk.stats().entries;
            if result["status"].as_str() == Some("ok") {
                assert_eq!(cached, 7);
                complete = true;
                break;
            }
            assert_eq!(result["status"].as_str(), Some("incomplete"), "{result:?}");
            assert!(cached > prior, "bounded query must make progress");
            prior = cached;
        }
        assert!(complete);
        let after = store.state.lock().unwrap().counters;
        assert!(after[0] - baseline[0] <= 7);
        assert_eq!(after[4] - baseline[4], 6);
    }

    #[test]
    fn prepared_deep_queries_preserve_root_window_and_whole_child() {
        let root_tool = r#"{"type":"assistant","uuid":"root-tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read-root","name":"Read","input":{"file_path":"root.rs"}}]}}"#;
        let child_tool = r#"{"type":"assistant","uuid":"child-tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read-child","name":"Read","input":{"file_path":"child.rs"}}]}}"#;
        let source = Source::new(&format!(
            "{}\n{root_tool}\n{}\n",
            user("first"),
            user("last")
        ));
        let child = source.directory.join("external.jsonl");
        std::fs::write(&child, format!("{}\n{child_tool}\n", user("child"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let prepared = finish(&store, store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[child.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner);
        assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
        let selectors = json!([{"kind":"current_turn"}]);
        for pattern in ["root.rs", "child.rs"] {
            let query = json!({"kind":"has_read","pattern":pattern,"subagents":true});
            let old = finish(
                &store,
                store.request(
                    &graph_request(
                        &root,
                        query.clone(),
                        vec![child.to_string_lossy().into_owned()],
                        selectors.clone(),
                    ),
                    &owner,
                    &Cancellation::default(),
                ),
                &owner,
            );
            let new = store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":prepared["data"]["handle"],"selectors":selectors,"query":query,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
            assert_eq!(new["status"].as_str(), Some("ok"), "{new:?}");
            assert_eq!(new["data"]["value"], old["data"]["value"]);
        }
        let graph_id = prepared["data"]["handle"]["graph_id"].as_str().unwrap();
        store.assert_conserved();
        let (slices, slice_bytes, before) = {
            let state = store.lock_state();
            let graph = state.prepared_graphs[graph_id].lock().unwrap();
            (
                graph.root_slices.len(),
                graph
                    .root_slices
                    .values()
                    .map(|facts| facts.accounted_bytes())
                    .sum::<usize>(),
                state.ledger.shared.facts(),
            )
        };
        assert_eq!(slices, 1);
        assert!(slice_bytes > 0);
        let released = store.request(&json!({"schema":SCHEMA,"id":"release-graph","operation":"release","kind":"graph","owner_epoch":store.owner_epoch,"token":graph_id}), &owner, &Cancellation::default());
        assert_eq!(
            released["data"]["released"].as_bool(),
            Some(true),
            "{released:?}"
        );
        store.assert_conserved();
        assert_eq!(
            store.lock_state().ledger.shared.facts(),
            before - slice_bytes
        );
    }

    #[test]
    fn prepared_large_source_reads_one_step_per_page() {
        let source = Source::new(&format!("{}\n", user("root")));
        let attachment = source.directory.join("large.jsonl");
        std::fs::write(&attachment, format!("{}\n", format!(r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#, "x".repeat(64 * 1024)))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024,"max_events_per_step":2,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let baseline = store.state.lock().unwrap().counters;
        let template = acquire(&source.path);
        let graph = store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[attachment.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let first = store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"), "{first:?}");
        assert!(first["cursor"].as_str().is_some());
        let after = store.state.lock().unwrap().counters;
        assert!(
            after[1] - baseline[1] <= 8 * 1024,
            "first page read {} bytes",
            after[1] - baseline[1]
        );
        let done = finish_prepared(&store, first, &owner);
        assert_eq!(done["status"].as_str(), Some("ok"), "{done:?}");
        assert_eq!(done["data"]["value"].as_bool(), Some(false));
    }

    #[test]
    fn prepared_registry_retries_advance_without_rereading_sources() {
        let source = Source::new(&format!("{}\n", user("root")));
        let ids: Vec<_> = (0..923).map(|index| format!("thread-{index:04}")).collect();
        let mut source_bytes = 0usize;
        for id in &ids {
            let contents = format!("{}\n", user(id));
            source_bytes += contents.len();
            std::fs::write(source.directory.join(format!("{id}.jsonl")), contents).unwrap();
        }
        let store = NativeStore::new(&json!({
            "max_read_bytes_per_step":4096,
            "max_events_per_step":2048,
            "max_retained_bytes":32*1024*1024,
            "reserved_hook_accounted_bytes":4096,
            "max_leases":16,
            "reserved_hook_leases":1
        }))
        .unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut template = acquire(&source.path);
        template.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        template["limits"].insert("max_source_read_bytes", json!(32 * 1024));
        template["limits"].insert("max_events", json!(1024));
        template["limits"].insert("max_items", json!(1024));
        template["limits"].insert("max_discovery_entries", json!(2048));
        template["limits"].insert("max_sources", json!(1024));
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let baseline = store.state.lock().unwrap().counters;
        let mut background = owner.clone();
        background.insert("work_class", json!("background"));
        let warm = store.request(&json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":prepare["thread_ids"],"roots":prepare["roots"],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":now_ms()+3_000,"limits":template["limits"]}), &background, &Cancellation::default());
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm["data"]["next_index"].as_u64(), Some(8));
        let after_warm = store.state.lock().unwrap().counters;
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert_eq!(store.state.lock().unwrap().counters[1], after_warm[1]);
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let mut completed = false;
        let mut attempts = 0;
        for _ in 0..10 {
            let before = store.state.lock().unwrap().counters;
            let reply = finish_prepared(
                &store,
                store.request(&query, &owner, &Cancellation::default()),
                &owner,
            );
            let after = store.state.lock().unwrap().counters;
            assert!(after[1] - before[1] <= 32 * 1024);
            attempts += 1;
            if reply["status"].as_str() == Some("ok") {
                assert_eq!(reply["data"]["value"].as_bool(), Some(false));
                completed = true;
                break;
            }
            assert_eq!(reply["status"].as_str(), Some("incomplete"), "{reply:?}");
            assert!(reply["cursor"].as_str().is_none());
        }
        assert!(completed);
        assert!(attempts > 1);
        let after = store.state.lock().unwrap().counters;
        assert!(after[0] - baseline[0] <= 929);
        assert!(after[1] - baseline[1] >= source_bytes as u64);
        assert!(after[1] - baseline[1] <= source_bytes as u64 + 128 * 929);
        let before_checks = store.membership_metadata_checks.load(Ordering::Relaxed);
        let mut verification = json!({"schema":SCHEMA,"id":"verify","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":prepare["thread_ids"],"roots":prepare["roots"],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let mut verified = false;
        for _ in 0..120 {
            let reply = store.request(&verification, &background, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert_eq!(
                reply["usage"]["discovery_entries_examined"].as_u64(),
                Some(0)
            );
            assert_eq!(reply["usage"]["source_bytes_read"].as_u64(), Some(0));
            if reply["data"]["complete"].as_bool() == Some(true) {
                verified = true;
                break;
            }
            verification.insert("start_index", reply["data"]["next_index"].clone());
            verification.insert(
                "membership_revision",
                reply["data"]["membership_revision"].clone(),
            );
        }
        assert!(verified);
        let checks = store.membership_metadata_checks.load(Ordering::Relaxed) - before_checks;
        assert!(
            checks <= 5 * 929,
            "warm verification made {checks} metadata checks"
        );
        let deadline = now_ms() + 750;
        let started = std::time::Instant::now();
        let before_query_bytes = store.state.lock().unwrap().counters[1];
        for query in [
            json!({"kind":"has_edit_to","values":["src/**"]}),
            json!({"kind":"has_read","pattern":"missing","subagents":true}),
        ] {
            let mut reply = store.request(
                &json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":query,"deadline_unix_ms":deadline,"limits":template["limits"]}),
                &owner,
                &Cancellation::default(),
            );
            let mut pages = 1;
            while reply["status"].as_str() == Some("incomplete") {
                let cursor = reply["cursor"].as_str().expect("cached query cursor");
                reply = store.request(
                    &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                    &owner,
                    &Cancellation::default(),
                );
                pages += 1;
            }
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert_eq!(reply["data"]["value"].as_bool(), Some(false));
            assert!(pages <= 4, "cached query took {pages} pages");
        }
        assert_eq!(store.state.lock().unwrap().counters[1], before_query_bytes);
        assert!(
            started.elapsed().as_millis() < 750,
            "two cached queries took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn prepared_graph_resumes_source_larger_than_each_call_budget() {
        let source = Source::new(&format!("{}\n", user("root")));
        let large = source.directory.join("large.jsonl");
        std::fs::write(&large, format!("{}\n", user(&"x".repeat(64 * 1024)))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut template = acquire(&source.path);
        template["limits"].insert("max_source_read_bytes", json!(16 * 1024));
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[large.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let before = store.state.lock().unwrap().counters;
        let mut completed = false;
        for _ in 0..8 {
            let reply = finish_prepared(
                &store,
                store.request(&query, &owner, &Cancellation::default()),
                &owner,
            );
            if reply["status"].as_str() == Some("ok") {
                assert_eq!(reply["data"]["value"].as_bool(), Some(false));
                completed = true;
                break;
            }
            assert_eq!(reply["status"].as_str(), Some("incomplete"), "{reply:?}");
            assert!(reply["cursor"].as_str().is_none());
        }
        assert!(completed);
        let after = store.state.lock().unwrap().counters;
        let source_bytes = std::fs::metadata(&large).unwrap().len();
        assert!(after[1] - before[1] >= source_bytes);
        assert!(after[1] - before[1] <= source_bytes + 128);
        assert_eq!(store.prepared_disk.stats().writes, 2);
    }

    fn appended_graph_query(
        child_contents: &str,
        cache_child: bool,
        allowance: u64,
    ) -> (Value, u64) {
        let source = Source::new(&format!("{}\n", user("root")));
        let child = source.directory.join("child.jsonl");
        std::fs::write(&child, child_contents).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":128,"max_events_per_step":100,"max_entry_bytes":8192,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        if cache_child {
            let cached = finish(
                &store,
                store.request(&acquire(&child), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(cached["status"].as_str(), Some("ok"), "{cached:?}");
        }
        let template = acquire(&source.path);
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[child.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        source.append(&format!("{}\n", user("appended")));
        let mut limits = template["limits"].clone();
        limits.insert("max_source_read_bytes", json!(allowance));
        let mut reply = store.request(
            &json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":limits}),
            &owner,
            &Cancellation::default(),
        );
        let mut read = reply["usage"]["source_bytes_read"].as_u64().unwrap();
        for _ in 0..4096 {
            let Some(cursor) = reply["cursor"].as_str().map(str::to_owned) else {
                return (reply, read);
            };
            reply = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
            read += reply["usage"]["source_bytes_read"].as_u64().unwrap();
        }
        panic!("prepared query did not finish");
    }

    #[test]
    fn prepared_query_pages_stay_within_the_request_source_allowance() {
        let (reply, read) =
            appended_graph_query(&format!("{}\n", user(&"x".repeat(4000))), false, 1200);
        assert_eq!(reply["status"].as_str(), Some("incomplete"), "{reply:?}");
        assert_eq!(
            reply["reason"].as_str(),
            Some("source_read_limit"),
            "{reply:?}"
        );
        assert!(
            read <= 1200,
            "read {read} source bytes against a 1200-byte allowance"
        );
    }

    #[test]
    fn prepared_query_reuses_a_cached_child_without_spare_source_allowance() {
        let child = format!("{}\n", user("child"));
        let (ample, needed) = appended_graph_query(&child, true, 1024 * 1024);
        assert_eq!(ample["status"].as_str(), Some("ok"), "{ample:?}");
        let (exact, read) = appended_graph_query(&child, true, needed);
        assert_eq!(exact["status"].as_str(), Some("ok"), "{exact:?}");
        assert_eq!(read, needed);
    }

    #[test]
    fn registered_warmer_resumes_large_source_and_writes_facts_once() {
        let source = Source::new(&format!("{}\n", user("root")));
        let large = source.directory.join("thread-large.jsonl");
        let small = source.directory.join("thread-small.jsonl");
        std::fs::write(&large, format!("{}\n", user(&"x".repeat(20 * 1024)))).unwrap();
        std::fs::write(&small, format!("{}\n", user("small"))).unwrap();
        let source_bytes =
            std::fs::metadata(&large).unwrap().len() + std::fs::metadata(&small).unwrap().len();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":["thread-large","thread-small"],"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        request["limits"].insert("max_source_read_bytes", json!(8192));
        let before = store.state.lock().unwrap().counters;
        let mut completed = false;
        for attempt in 0..16 {
            let reply = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert!(reply["usage"]["source_bytes_read"].as_u64().unwrap() <= 8192);
            if attempt > 0 {
                assert_eq!(
                    reply["usage"]["discovery_entries_examined"].as_u64(),
                    Some(0)
                );
            }
            if reply["data"]["complete"].as_bool() == Some(true) {
                assert_eq!(reply["data"]["next_index"].as_u64(), Some(2));
                completed = true;
                break;
            }
            request.insert("start_index", reply["data"]["next_index"].clone());
            request.insert(
                "membership_revision",
                reply["data"]["membership_revision"].clone(),
            );
        }
        assert!(completed);
        let after = store.state.lock().unwrap().counters;
        assert!(after[1] - before[1] >= source_bytes);
        assert!(after[1] - before[1] <= source_bytes + 256);
        assert_eq!(store.prepared_disk.stats().writes, 2);
        request.insert("start_index", json!(0));
        let warm = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm["data"]["complete"].as_bool(), Some(true));
        assert_eq!(warm["usage"]["source_bytes_read"].as_u64(), Some(0));
        assert_eq!(warm["data"]["fact_cache_writes"].as_u64(), Some(2));
        let mut other_claimant = context("another-session");
        other_claimant.insert("work_class", json!("background"));
        let shared = store.request(&request, &other_claimant, &Cancellation::default());
        assert_eq!(shared["status"].as_str(), Some("ok"), "{shared:?}");
        assert_eq!(shared["data"]["complete"].as_bool(), Some(true));
        assert_eq!(shared["usage"]["source_bytes_read"].as_u64(), Some(0));
    }

    #[test]
    fn sidechain_disk_fact_hits_are_promoted_into_memory() {
        let source = Source::new(&format!("{}\n", user("root")));
        let sidechain = source.directory.join("side.jsonl");
        std::fs::write(&sidechain, format!("{}\n", user("side"))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[sidechain.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = finish_prepared(
            &store,
            store.request(&query, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["data"]["value"].as_bool(), Some(false), "{first:?}");
        let identity = SourceStamp::of(&std::fs::metadata(&sidechain).unwrap()).identity;
        store.lock_state().remove_prepared_facts(&identity).unwrap();
        let writes = store.prepared_disk.stats().writes;
        let before = store.state.lock().unwrap().counters;
        let from_disk = finish_prepared(
            &store,
            store.request(&query, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            from_disk["data"]["value"].as_bool(),
            Some(false),
            "{from_disk:?}"
        );
        assert_eq!(store.state.lock().unwrap().counters[1], before[1]);
        assert_eq!(store.prepared_disk.stats().writes, writes);
        assert!(store
            .state
            .lock()
            .unwrap()
            .prepared_facts
            .contains_key(&identity));
    }

    #[test]
    fn oversize_disk_fact_hits_leave_memory_facts_cached() {
        let source = Source::new(&format!("{}\n", user("root")));
        let small = source.directory.join("small.jsonl");
        std::fs::write(&small, format!("{}\n", user("side"))).unwrap();
        let large = source.directory.join("large.jsonl");
        std::fs::write(&large, format!("{}\n", format!(r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#, "x".repeat(64 * 1024)))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":32*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let template = acquire(&source.path);
        let graph = |path: &std::path::Path| {
            let root = finish(
                &store,
                store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                &owner,
            );
            let graph = finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[path.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
            json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]})
        };
        let ask = |query: &Value| {
            let result = finish_prepared(
                &store,
                store.request(query, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(result["data"]["value"].as_bool(), Some(false), "{result:?}");
        };
        let identity =
            |path: &std::path::Path| SourceStamp::of(&std::fs::metadata(path).unwrap()).identity;
        let large_query = graph(&large);
        ask(&large_query);
        ask(&graph(&small));
        assert!(!store
            .state
            .lock()
            .unwrap()
            .prepared_facts
            .contains_key(&identity(&large)));
        assert!(store
            .state
            .lock()
            .unwrap()
            .prepared_facts
            .contains_key(&identity(&small)));
        let writes = store.prepared_disk.stats().writes;
        let before = store.state.lock().unwrap().counters;
        ask(&large_query);
        assert_eq!(store.state.lock().unwrap().counters[1], before[1]);
        assert_eq!(store.prepared_disk.stats().writes, writes);
        assert!(store
            .state
            .lock()
            .unwrap()
            .prepared_facts
            .contains_key(&identity(&small)));
    }

    #[test]
    fn registered_warmer_advances_a_forty_megabyte_source_in_eight_megabyte_steps() {
        let source = Source::new(&format!("{}\n", user("root")));
        let large = source.directory.join("large.jsonl");
        let content = format!(
            r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#,
            "x".repeat(40 * 1024 * 1024)
        );
        std::fs::write(&large, format!("{content}\n")).unwrap();
        let source_bytes = std::fs::metadata(&large).unwrap().len();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_retained_bytes":512*1024*1024,"max_entry_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":[],"roots":[],"direct_paths":[large.to_string_lossy().as_ref()],"start_index":0,"membership_revision":null,"deadline_unix_ms":now_ms()+120_000,"limits":template["limits"]});
        request["limits"].insert("max_source_read_bytes", json!(8 * 1024 * 1024));
        let before = store.state.lock().unwrap().counters;
        let mut previous_offset = 0u64;
        let mut advanced = false;
        let mut completed = false;
        for _ in 0..12 {
            let reply = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert!(reply["usage"]["source_bytes_read"].as_u64().unwrap() <= 8 * 1024 * 1024);
            if reply["data"]["complete"].as_bool() == Some(true) {
                completed = true;
                break;
            }
            let offset = reply["data"]["source_offset"].as_u64().unwrap();
            assert!(offset >= previous_offset);
            advanced |= offset > previous_offset;
            previous_offset = offset;
            request.insert(
                "membership_revision",
                reply["data"]["membership_revision"].clone(),
            );
        }
        assert!(advanced);
        assert!(completed);
        let after = store.state.lock().unwrap().counters;
        assert!(after[1] - before[1] >= source_bytes);
        assert!(after[1] - before[1] <= source_bytes + 256);
        assert_eq!(store.prepared_disk.stats().writes, 1);
    }

    #[test]
    fn codex_append_reads_only_new_bytes_and_bounded_fences() {
        let first = r#"{"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"s","cwd":"/tmp"}}"#;
        let body = format!(
            r#"{{"timestamp":"2026-01-02T03:04:06Z","type":"event_msg","payload":{{"type":"agent_message","message":"{}"}}}}"#,
            "x".repeat(2 * 1024 * 1024)
        );
        let source = Source::new(&format!("{first}\n{body}\n"));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":4*1024*1024,"max_retained_bytes":256*1024*1024,"max_entry_bytes":4*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":[],"roots":[],"direct_paths":[source.path.to_string_lossy().as_ref()],"start_index":0,"membership_revision":null,"deadline_unix_ms":now_ms()+120_000,"limits":template["limits"]});
        request["limits"].insert("max_source_read_bytes", json!(4 * 1024 * 1024));
        let warm = |store: &NativeStore, request: &mut Value| {
            let mut bytes = 0u64;
            for _ in 0..12 {
                let reply = store.request(request, &owner, &Cancellation::default());
                assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
                bytes += reply["usage"]["source_bytes_read"].as_u64().unwrap();
                if reply["data"]["complete"].as_bool() == Some(true) {
                    return bytes;
                }
                request.insert("start_index", reply["data"]["next_index"].clone());
                request.insert(
                    "membership_revision",
                    reply["data"]["membership_revision"].clone(),
                );
            }
            panic!("codex warming did not complete");
        };
        let first_bytes = warm(&store, &mut request);
        assert!(first_bytes >= std::fs::metadata(&source.path).unwrap().len());
        for index in 0..3 {
            let appended = format!(
                r#"{{"timestamp":"2026-01-02T03:04:0{}Z","type":"event_msg","payload":{{"type":"agent_message","message":"small-{index}"}}}}"#,
                index + 7
            ) + "\n";
            source.append(&appended);
            request.insert("start_index", json!(0));
            request.insert("membership_revision", Value::new_null());
            let before_lowered = store.state.lock().unwrap().counters[16];
            let read = warm(&store, &mut request);
            assert!(read >= appended.len() as u64);
            assert!(read <= appended.len() as u64 + 256);
            let after_lowered = store.state.lock().unwrap().counters[16];
            assert_eq!(after_lowered, before_lowered);
        }
    }

    #[test]
    fn registered_warmer_rejects_changed_or_reordered_membership() {
        let source = Source::new(&format!("{}\n", user("root")));
        let first = source.directory.join("thread-a.jsonl");
        let second = source.directory.join("thread-b.jsonl");
        std::fs::write(&first, format!("{}\n", user("a"))).unwrap();
        std::fs::write(&second, format!("{}\n", user("b"))).unwrap();
        let store = store();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":["thread-a","thread-b"],"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        request["limits"].insert(
            "max_source_read_bytes",
            json!(std::fs::metadata(&first).unwrap().len() + 128),
        );
        let mut partial = Value::new_null();
        for _ in 0..12 {
            partial = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(partial["status"].as_str(), Some("ok"), "{partial:?}");
            if partial["data"]["next_index"].as_u64() == Some(1) {
                break;
            }
            request.insert(
                "membership_revision",
                partial["data"]["membership_revision"].clone(),
            );
        }
        assert_eq!(partial["status"].as_str(), Some("ok"), "{partial:?}");
        assert_eq!(partial["data"]["next_index"].as_u64(), Some(1));
        request.insert("start_index", json!(1));
        request.insert(
            "membership_revision",
            partial["data"]["membership_revision"].clone(),
        );
        request.insert("thread_ids", json!(["thread-b", "thread-a"]));
        let reordered = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(
            reordered["status"].as_str(),
            Some("changed"),
            "{reordered:?}"
        );
        request.insert("thread_ids", json!(["thread-a", "thread-b"]));
        std::fs::write(&first, format!("{}\n", user("changed-longer"))).unwrap();
        let changed = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(changed["status"].as_str(), Some("changed"), "{changed:?}");
    }

    #[test]
    fn background_warming_preserves_a_foreground_load_slot() {
        let source = Source::new(&format!("{}\n", user("root")));
        let large = source.directory.join("large.jsonl");
        let other = source.directory.join("other.jsonl");
        std::fs::write(&large, format!("{}\n", user(&"x".repeat(32 * 1024)))).unwrap();
        std::fs::write(&other, format!("{}\n", user("other"))).unwrap();
        let store = NativeStore::new(&json!({"max_pending_loads":2,"reserved_hook_loads":1,"max_read_bytes_per_step":1024,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let foreground = context("a");
        let mut background = foreground.clone();
        background.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let request = |path: &Path| json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":[],"roots":[],"direct_paths":[path.to_string_lossy().as_ref()],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let partial = store.request(&request(&large), &background, &Cancellation::default());
        assert_eq!(partial["status"].as_str(), Some("ok"), "{partial:?}");
        assert_eq!(partial["data"]["next_index"].as_u64(), Some(0));
        let refused = store.request(&request(&other), &background, &Cancellation::default());
        assert_eq!(
            refused["status"].as_str(),
            Some("retained_limit"),
            "{refused:?}"
        );
        let accepted = store.request(&acquire(&other), &foreground, &Cancellation::default());
        assert_ne!(
            accepted["status"].as_str(),
            Some("retained_limit"),
            "{accepted:?}"
        );
    }

    #[test]
    fn timed_out_warm_steps_report_partial_source_progress() {
        let source = Source::new(&format!("{}\n", user("root")));
        let large = source.directory.join("large.jsonl");
        std::fs::write(&large, format!("{}\n", user(&"x".repeat(32 * 1024)))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        *store.read_hook.lock().unwrap() = Some(Arc::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(150));
        }));
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":[],"roots":[],"direct_paths":[large.to_string_lossy().as_ref()],"start_index":0,"membership_revision":null,"deadline_unix_ms":now_ms()+100,"limits":template["limits"]});
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert!(first["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
        request.insert(
            "membership_revision",
            first["data"]["membership_revision"].clone(),
        );
        request.insert("deadline_unix_ms", json!(now_ms() + 100));
        let second = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert!(second["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
        assert!(
            second["data"]["source_offset"].as_u64().unwrap()
                > first["data"]["source_offset"].as_u64().unwrap()
        );
    }

    #[test]
    fn disk_facts_complete_queries_when_memory_fact_budget_is_smaller() {
        let source = Source::new(&format!("{}\n", user("root")));
        let mut paths = Vec::new();
        let mut source_bytes = 0u64;
        for index in 0..3 {
            let path = source.directory.join(format!("external-{index}.jsonl"));
            let content = format!(
                r#"{{"type":"user","uuid":"external-{index}","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#,
                "x".repeat(64 * 1024)
            );
            std::fs::write(&path, format!("{content}\n")).unwrap();
            source_bytes += std::fs::metadata(&path).unwrap().len();
            paths.push(path.to_string_lossy().into_owned());
        }
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":32*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let before = store.state.lock().unwrap().counters;
        let first = finish_prepared(
            &store,
            store.request(&query, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(first["data"]["value"].as_bool(), Some(false));
        let after_first = store.state.lock().unwrap().counters;
        assert!(after_first[1] - before[1] >= source_bytes);
        assert!(after_first[1] - before[1] <= source_bytes + 384);
        assert_eq!(store.prepared_disk.stats().writes, 4);
        let second = finish_prepared(
            &store,
            store.request(&query, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert_eq!(second["data"]["value"].as_bool(), Some(false));
        let after_second = store.state.lock().unwrap().counters;
        assert_eq!(after_second[1], after_first[1]);
        assert_eq!(store.prepared_disk.stats().writes, 4);
    }

    #[test]
    fn changed_prepared_facts_use_separate_disk_revisions() {
        let source = Source::new(&format!("{}\n", user("root")));
        let attachment = source.directory.join("large.jsonl");
        let write = |text: &str| {
            std::fs::write(&attachment, format!("{}\n", format!(r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{text}"}}}}"#))).unwrap()
        };
        write(&"a".repeat(48 * 1024));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[attachment.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = finish_prepared(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let query = |graph: &Value| {
            finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph,"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            )
        };
        assert_eq!(
            query(&first["data"]["handle"])["status"].as_str(),
            Some("ok")
        );
        write(&"b".repeat(56 * 1024));
        let second = finish_prepared(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert_eq!(
            query(&second["data"]["handle"])["status"].as_str(),
            Some("ok")
        );
        assert_eq!(
            query(&first["data"]["handle"])["status"].as_str(),
            Some("changed")
        );
        assert_eq!(store.prepared_disk.stats().writes, 3);
    }

    #[test]
    fn cancelling_one_prepared_waiter_keeps_another_source_load() {
        let source = Source::new(&format!("{}\n", user("root")));
        let attachment = source.directory.join("large.jsonl");
        std::fs::write(&attachment, format!("{}\n", format!(r#"{{"type":"user","uuid":"large","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{}"}}}}"#, "x".repeat(32 * 1024)))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024,"max_events_per_step":2,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[attachment.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let graph = finish_prepared(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = store.request(&query, &owner, &Cancellation::default());
        let second = store.request(&query, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"));
        assert_eq!(second["status"].as_str(), Some("incomplete"));
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let rejected = store.request(
            &json!({"schema":SCHEMA,"id":"cancel","operation":"resume","cursor":first["cursor"]}),
            &owner,
            &cancelled,
        );
        assert_eq!(
            rejected["status"].as_str(),
            Some("cancelled"),
            "{rejected:?}"
        );
        let done = finish_prepared(&store, second, &owner);
        assert_eq!(done["status"].as_str(), Some("ok"), "{done:?}");
    }

    #[test]
    fn prepared_graph_rejects_authority_change_on_resume_query_and_release() {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths: Vec<_> = (0..10)
            .map(|index| {
                let path = source.directory.join(format!("external-{index}.jsonl"));
                std::fs::write(&path, format!("{}\n", user(&format!("external-{index}")))).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("incomplete"));
        let mut narrowed = owner.clone();
        narrowed.insert("authority", json!({"kind":"restricted_roots","effective_uid":unsafe { libc::geteuid() }.to_string(),"roots":[source.directory.to_string_lossy().as_ref()]}));
        let denied = store.request(
            &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":first["cursor"]}),
            &narrowed,
            &Cancellation::default(),
        );
        assert_eq!(
            denied["status"].as_str(),
            Some("stale_cursor"),
            "{denied:?}"
        );
        let complete = finish_prepared(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
        let graph = &complete["data"]["handle"];
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph,"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let denied = store.request(&query, &narrowed, &Cancellation::default());
        assert_eq!(
            denied["status"].as_str(),
            Some("stale_handle"),
            "{denied:?}"
        );
        let warmed = finish_prepared(
            &store,
            store.request(&query, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(warmed["status"].as_str(), Some("ok"), "{warmed:?}");
        let release = json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"graph","token":graph["graph_id"],"owner_epoch":graph["owner_epoch"]});
        let denied = store.request(&release, &narrowed, &Cancellation::default());
        assert_eq!(
            denied["status"].as_str(),
            Some("stale_handle"),
            "{denied:?}"
        );
        let allowed = store.request(&release, &owner, &Cancellation::default());
        assert_eq!(allowed["status"].as_str(), Some("ok"), "{allowed:?}");
        let before = store.state.lock().unwrap().counters;
        let narrowed_graph = finish_prepared(
            &store,
            store.request(&request, &narrowed, &Cancellation::default()),
            &narrowed,
        );
        assert_eq!(
            narrowed_graph["status"].as_str(),
            Some("ok"),
            "{narrowed_graph:?}"
        );
        let narrowed_query = json!({"schema":SCHEMA,"id":"narrowed-query","operation":"query_graph","handle":narrowed_graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let narrowed_result = finish_prepared(
            &store,
            store.request(&narrowed_query, &narrowed, &Cancellation::default()),
            &narrowed,
        );
        assert_eq!(
            narrowed_result["status"].as_str(),
            Some("ok"),
            "{narrowed_result:?}"
        );
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[0] - before[0], 10);
    }

    #[test]
    fn prepared_root_facts_are_keyed_by_classifier() {
        let source = Source::new(&format!("{}\n", user("root")));
        let store = store();
        let owner = context("a");
        store
            .register_classifier(
                "none",
                "1",
                Arc::new(|_, range| Ok(vec![false; range.len()])),
            )
            .unwrap();
        let native = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut custom_request = acquire(&source.path);
        custom_request.insert("classifier", json!({"id":"none","version":"1"}));
        let custom = finish(
            &store,
            store.request(&custom_request, &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let prepare = |root: &Value, classifier: Value| {
            finish_prepared(&store, store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(root),"classifier":classifier,"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner)
        };
        let first = prepare(&native, json!({"id":"native","version":"1"}));
        let second = prepare(&custom, json!({"id":"none","version":"1"}));
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        let state = store.state.lock().unwrap();
        let first_graph = state.prepared_graphs
            [first["data"]["handle"]["graph_id"].as_str().unwrap()]
        .lock()
        .unwrap();
        let second_graph = state.prepared_graphs
            [second["data"]["handle"]["graph_id"].as_str().unwrap()]
        .lock()
        .unwrap();
        assert!(!Arc::ptr_eq(
            &first_graph.root_facts,
            &second_graph.root_facts
        ));
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
    fn graph_predicate_scans_918_attachments_without_retaining_member_leases() {
        let source = Source::new(&format!("{}\n", user("root")));
        let attachments: Vec<_> = (0..918)
            .map(|index| {
                let path = source
                    .directory
                    .join(format!("attachment-{index:04}.jsonl"));
                std::fs::write(&path, format!("{}\n", user(&format!("attachment-{index}"))))
                    .unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let store = NativeStore::new(&json!({
            "max_retained_bytes":128*1024*1024,
            "reserved_hook_accounted_bytes":4096,
            "max_leases":256,
            "reserved_hook_leases":1,
            "max_events_per_step":256
        }))
        .unwrap();
        let owner = context("a");
        let deadline = now_ms() + 120_000;
        let mut root_request = acquire(&source.path);
        root_request.insert("deadline_unix_ms", json!(deadline));
        let root = finish(
            &store,
            store.request(&root_request, &owner, &Cancellation::default()),
            &owner,
        );
        let mut request = graph_request(
            &root,
            json!({"kind":"has_read","pattern":"absent.rs","subagents":true}),
            attachments,
            json!([]),
        );
        request["limits"].insert("max_sources", json!(1024));
        request["limits"].insert("max_discovery_entries", json!(1024));
        request["limits"].insert("max_events", json!(4096));
        request["limits"].insert("max_source_read_bytes", json!(8 * 1024 * 1024));
        request["limits"].insert("max_items", json!(1024));
        request.insert("deadline_unix_ms", json!(deadline));
        let mut reply = store.request(&request, &owner, &Cancellation::default());
        let mut peak_leases = store.state.lock().unwrap().leases.len();
        for _ in 0..1200 {
            if reply["status"].as_str() != Some("incomplete") {
                break;
            }
            let cursor = reply["cursor"]
                .as_str()
                .unwrap_or_else(|| panic!("bounded graph continuation: {reply:?}"));
            reply = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
            peak_leases = peak_leases.max(store.state.lock().unwrap().leases.len());
        }
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(reply["data"]["value"].as_bool(), Some(false));
        assert!(peak_leases <= 2, "peak leases: {peak_leases}");
        assert_eq!(store.state.lock().unwrap().leases.len(), 1);
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
        let carried = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
            )
            .unwrap();
        assert_eq!(carried["complete"].as_bool(), Some(true));
        assert!(carried["cursor"].is_null());
        assert_ne!(
            carried["description"]["handle"]["generation"],
            complete["description"]["handle"]["generation"]
        );
        let page2 = store
            .prepare_classifier(
                handle(&root),
                &json!({"id":"application","version":"2"}),
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
        let generations = store.state.lock().unwrap().generations.len();
        let failed = store
            .prepare_classifier(
                handle(&root),
                &classifier,
                &owner,
                &Cancellation::default(),
                bounds,
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
            let mut lease = state
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
            let mut lease = state
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

    fn release_lease(store: &NativeStore, handle: &Value, owner: &Value) {
        let released = store.request(
            &json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"lease","owner_epoch":store.owner_epoch,"token":handle["lease_id"]}),
            owner,
            &Cancellation::default(),
        );
        assert_eq!(
            released["data"]["released"].as_bool(),
            Some(true),
            "{released:?}"
        );
    }

    fn entry_bytes(store: &NativeStore) -> usize {
        let mut state = store.lock_state();
        number(
            &NativeStore::gauges(&mut state),
            "retained_entry_capacity_bytes",
        )
        .unwrap()
    }

    #[test]
    fn a_shared_chunk_prefix_is_released_exactly_once() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let first = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prior = store.pin(handle(&first), &owner).unwrap();
        source.append(&format!("{}\n", user("b")));
        let second = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let current = store.pin(handle(&second), &owner).unwrap();
        assert!(Arc::ptr_eq(&prior.chunks[0], &current.chunks[0]));
        store.assert_conserved();
        let before = entry_bytes(&store);
        let prior_own = prior.accounted_allocations()[0].1;
        release_lease(&store, handle(&first), &owner);
        drop(prior);
        store.assert_conserved();
        assert_eq!(
            entry_bytes(&store),
            before - prior_own.owned_capacity_bytes - prior_own.opaque_dom_accounted_bytes
        );
        release_lease(&store, handle(&second), &owner);
        store.lock_state().retain_latest(|_, _| false);
        drop(current);
        store.assert_conserved();
        assert_eq!(entry_bytes(&store), 0);
        assert_eq!(
            number(
                &NativeStore::gauges(&mut store.lock_state()),
                "live_generations"
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn escaped_rows_stay_charged_until_their_last_owner_drops() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let response = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        let rows = Arc::clone(&snapshot.chunks[0].entries);
        let rows_bytes = snapshot.chunks[0].charge.owned_capacity_bytes
            + snapshot.chunks[0].charge.opaque_dom_accounted_bytes;
        release_lease(&store, handle(&response), &owner);
        store.lock_state().retain_latest(|_, _| false);
        drop(snapshot);
        store.assert_conserved();
        assert_eq!(entry_bytes(&store), rows_bytes);
        drop(rows);
        store.assert_conserved();
        assert_eq!(entry_bytes(&store), 0);
        assert!(store.lock_state().escaped_chunks.is_empty());
    }

    #[test]
    fn a_completed_classifier_stage_is_pruned_conservatively() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let owner = context("a");
        store
            .register_classifier(
                "custom",
                "1",
                Arc::new(|_, range| Ok(vec![true; range.len()])),
            )
            .unwrap();
        let mut request = acquire(&source.path);
        request.insert("classifier", json!({"id":"custom","version":"1"}));
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        store.assert_conserved();
        let stats = store.request(
            &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(stats["status"].as_str(), Some("ok"), "{stats:?}");
        assert!(store.lock_state().classifier_stages.is_empty());
        store.assert_conserved();
        release_lease(&store, handle(&response), &owner);
        store.lock_state().retain_latest(|_, _| false);
        store.classified.lock().unwrap().clear();
        store.assert_conserved();
        store.lock_state().retain_carried(|_, _| false);
        store.assert_conserved();
        assert_eq!(entry_bytes(&store), 0);
    }

    #[test]
    fn a_last_drop_under_the_state_lock_does_not_deadlock() {
        let source = Source::new(&format!("{}\n", user("a")));
        let store = store();
        let owner = context("a");
        let response = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        let contended = Arc::clone(&snapshot);
        release_lease(&store, handle(&response), &owner);
        let before = store.audits.load(Ordering::Relaxed);
        {
            let mut state = store.lock_state();
            state.retain_latest(|_, _| false);
            drop(snapshot);
            let (sender, receiver) = std::sync::mpsc::channel();
            let dropper = std::thread::spawn(move || {
                drop(contended);
                sender.send(()).unwrap();
            });
            receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the last drop completes while the state lock is held");
            dropper.join().unwrap();
            assert_eq!(
                number(&NativeStore::gauges(&mut state), "live_generations").unwrap(),
                0
            );
        }
        store.assert_conserved();
        assert_eq!(entry_bytes(&store), 0);
        assert_eq!(store.audits.load(Ordering::Relaxed), before + 1);
        assert!(store.retained_work.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn pending_load_charges_are_conserved_across_steps() {
        let source = Source::new(&format!(
            "{}\n{}\n{}\n{}\n",
            user("a"),
            user("b"),
            user("c"),
            user("d")
        ));
        let store = store();
        let owner = context("a");
        let mut response = store.request(&acquire(&source.path), &owner, &Cancellation::default());
        let mut steps = 0;
        while response["status"].as_str() == Some("incomplete") {
            store.assert_conserved();
            let cursor = response["cursor"].as_str().unwrap().to_owned();
            response = store.request(
                &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
            steps += 1;
        }
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert!(steps > 1);
        store.assert_conserved();
        assert_eq!(store.lock_state().ledger.pending, 0);
    }

    const REPLY_RESERVATION: usize = MAX_REPLY_BYTES * 2;

    fn settled_bytes(store: &NativeStore) -> usize {
        let mut state = store.lock_state();
        number(
            &NativeStore::gauges(&mut state),
            "retained_total_accounted_bytes",
        )
        .unwrap()
            - NativeStore::fixed_metadata_bytes(&state)
    }

    fn admission_owner(background: bool) -> (Value, usize) {
        let mut owner = context("a");
        if background {
            owner.insert("work_class", json!("background"));
        }
        let cap = 32 * 1024 * 1024 - if background { 4096 } else { 0 };
        (owner, cap)
    }

    fn prepare_request(root: &Value) -> Value {
        let template = acquire(Path::new("/unused"));
        json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]})
    }

    fn release_graph(store: &NativeStore, prepared: &Value, owner: &Value) {
        let released = store.request(
            &json!({"schema":SCHEMA,"id":"release-graph","operation":"release","kind":"graph","owner_epoch":store.owner_epoch,"token":prepared["data"]["handle"]["graph_id"]}),
            owner,
            &Cancellation::default(),
        );
        assert_eq!(
            released["data"]["released"].as_bool(),
            Some(true),
            "{released:?}"
        );
    }

    fn assert_refused(store: &NativeStore, response: &Value) {
        assert_eq!(
            response["status"].as_str(),
            Some("retained_limit"),
            "{response:?}"
        );
        assert_eq!(
            response["reason"].as_str(),
            Some("accounted storage admission exhausted")
        );
        store.assert_conserved();
    }

    #[test]
    fn a_parked_prepared_query_is_charged_beyond_its_struct_and_query_strings() {
        let source = Source::new(&format!("{}\n", user("root")));
        let paths: Vec<String> = (0..12)
            .map(|index| {
                let path = source.directory.join(format!("external-{index:02}.jsonl"));
                let tool = format!(
                    r#"{{"type":"assistant","uuid":"tool-{index}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"bash-{index}","name":"Bash","input":{{"command":"echo {index}"}}}}]}}}}"#
                );
                std::fs::write(&path, format!("{}\n{tool}\n", user(&format!("u-{index}")))).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let prepare = finish_prepared(&store, store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()), &owner);
        assert_eq!(prepare["status"].as_str(), Some("ok"), "{prepare:?}");
        let page = store.request(&json!({"schema":SCHEMA,"id":"inputs","operation":"query_graph","handle":prepare["data"]["handle"],"selectors":[],"query":{"kind":"deep_predicate_inputs","order":"forward"},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default());
        assert_eq!(page["status"].as_str(), Some("incomplete"), "{page:?}");
        store.assert_conserved();
        let state = store.lock_state();
        let cursor = state
            .prepared_queries
            .values()
            .next()
            .expect("parked prepared query");
        let strings = crate::snapshot_memory::value_charge(&cursor.query).owned_capacity_bytes
            + cursor.input_records.as_ref().map_or(0, |records| {
                records.iter().map(String::capacity).sum::<usize>()
            });
        assert!(state.prepared_queries.charged() > size_of::<PreparedQueryCursor>() + strings);
    }

    #[test]
    fn expired_carried_classifications_are_pruned_through_the_index() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let owner = context("a");
        store
            .register_classifier(
                "custom",
                "1",
                Arc::new(|_, range| Ok(vec![true; range.len()])),
            )
            .unwrap();
        let mut request = acquire(&source.path);
        request.insert("classifier", json!({"id":"custom","version":"1"}));
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert!(!store.lock_state().carried_classifications.is_empty());
        let before = store.retained_work.load(Ordering::Relaxed);
        NativeStore::prune_at(&mut store.lock_state(), now_ms() + TOUCH_TTL_MS + 1);
        assert!(store.lock_state().carried_classifications.is_empty());
        assert!(store.retained_work.load(Ordering::Relaxed) > before);
        store.assert_conserved();
    }

    #[test]
    fn carried_expiry_tickets_charge_their_lineage_strings_until_compaction() {
        let source = Source::new(&format!("{}\n{}\n", user("a"), user("b")));
        let store = store();
        let owner = context("a");
        let classifier = "c".repeat(16 * 1024);
        store
            .register_classifier(
                &classifier,
                "1",
                Arc::new(|_, range| Ok(vec![true; range.len()])),
            )
            .unwrap();
        let mut request = acquire(&source.path);
        request.insert("classifier", json!({"id":classifier,"version":"1"}));
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        store.assert_conserved();
        let (lineage, carried) = {
            let state = store.lock_state();
            let (lineage, carried) = state
                .carried_classifications
                .iter()
                .next()
                .expect("carried classification");
            (lineage.clone(), Arc::clone(carried))
        };
        let ticket = lineage.owned_bytes();
        assert!(ticket >= 16 * 1024);
        let one_ticket = store.retained_accounted_bytes();
        {
            let state = store.lock_state();
            assert_eq!(state.carried_expiry.len(), 1);
            assert_eq!(state.carried_expiry.key_bytes(), ticket);
        }
        store
            .lock_state()
            .insert_carried(lineage.clone(), Arc::clone(&carried));
        store.assert_conserved();
        let heap_two = {
            let state = store.lock_state();
            assert_eq!(state.carried_expiry.len(), 2);
            assert_eq!(state.carried_expiry.key_bytes(), 2 * ticket);
            state.carried_expiry.heap_bytes()
        };
        let two_tickets = store.retained_accounted_bytes();
        assert_eq!(two_tickets, one_ticket + ticket);
        store
            .lock_state()
            .insert_carried(lineage.clone(), Arc::clone(&carried));
        store.assert_conserved();
        let heap_one = {
            let state = store.lock_state();
            assert_eq!(state.carried_expiry.len(), 1);
            assert_eq!(state.carried_expiry.key_bytes(), ticket);
            state.carried_expiry.heap_bytes()
        };
        eprintln!("carried tickets: lineage={ticket} heap_two={heap_two} heap_one={heap_one}");
        assert_eq!(
            store.retained_accounted_bytes(),
            two_tickets - (heap_two - heap_one)
        );
    }

    fn located(expires: u64) -> LocatedPath {
        LocatedPath {
            path: PathBuf::from("/locations/session.jsonl"),
            expires,
        }
    }

    #[test]
    fn location_eviction_pops_the_earliest_expiring_ticket() {
        let store = store();
        let owner = context("a");
        let horizon = now_ms() + 600_000;
        {
            let mut state = store.lock_state();
            for index in 0..4096u64 {
                state.insert_location(format!("session-{index:04}"), located(horizon + index));
            }
            assert_eq!(state.locations.len(), 4096);
        }
        let before = store.retained_work.load(Ordering::Relaxed);
        store.remember_locations(
            &[("fresh".to_owned(), PathBuf::from("/locations/fresh.jsonl"))],
            &owner,
        );
        {
            let state = store.lock_state();
            assert_eq!(state.locations.len(), 4096);
            assert!(!state.locations.contains_key("session-0000"));
            assert!(state.locations.contains_key("session-0001"));
            assert!(state.locations.contains_key("fresh"));
        }
        assert!(store.retained_work.load(Ordering::Relaxed) > before);
        store.assert_conserved();
    }

    #[test]
    fn an_extended_location_survives_its_stale_ticket_and_tickets_compact() {
        let store = store();
        {
            let mut state = store.lock_state();
            state.insert_location("session".to_owned(), located(now_ms() - 1));
            state.insert_location("session".to_owned(), located(now_ms() + 600_000));
            NativeStore::prune(&mut state);
            assert!(state.locations.contains_key("session"));
            for step in 1..=8u64 {
                state.insert_location("session".to_owned(), located(now_ms() + 600_000 + step));
            }
            assert_eq!(state.locations.len(), 1);
            assert!(state.locations_expiry.len() <= 2);
            state.insert_location("session".to_owned(), located(now_ms() - 1));
            NativeStore::prune(&mut state);
            assert!(state.locations.is_empty());
        }
        store.assert_conserved();
    }

    #[test]
    fn prepared_graph_publication_is_admitted_before_it_lands() {
        for background in [false, true] {
            let source = Source::new(&format!("{}\n", user("root")));
            let store = store();
            let (owner, cap) = admission_owner(background);
            let root = finish(
                &store,
                store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                &owner,
            );
            let prepare = prepare_request(&root);
            let warm = finish(
                &store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            release_graph(&store, &warm, &owner);
            let idle = store.retained_accounted_bytes();
            let measured = finish(
                &store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(measured["status"].as_str(), Some("ok"), "{measured:?}");
            let graph_bytes = store.retained_accounted_bytes() - idle;
            assert!(graph_bytes > 0);
            release_graph(&store, &measured, &owner);
            assert_eq!(store.retained_accounted_bytes(), idle);
            let room = cap - idle - graph_bytes - REPLY_RESERVATION;
            let crowded = store.reserve_projection(&owner, room + 1).unwrap();
            let refused = store.request(&prepare, &owner, &Cancellation::default());
            assert_refused(&store, &refused);
            assert_eq!(store.retained_accounted_bytes(), idle + room + 1);
            assert!(store.lock_state().prepared_graphs.is_empty());
            drop(crowded);
            let fitted = store.reserve_projection(&owner, room).unwrap();
            let exact = finish(
                &store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(exact["status"].as_str(), Some("ok"), "{exact:?}");
            assert_eq!(store.retained_accounted_bytes(), cap - REPLY_RESERVATION);
            store.assert_conserved();
            release_graph(&store, &exact, &owner);
            drop(fitted);
            assert_eq!(store.retained_accounted_bytes(), idle);
        }
    }

    #[test]
    fn root_slice_publication_is_admitted_before_it_lands() {
        let root_tool = r#"{"type":"assistant","uuid":"root-tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read-root","name":"Read","input":{"file_path":"root.rs"}}]}}"#;
        for background in [false, true] {
            let source = Source::new(&format!(
                "{}\n{root_tool}\n{}\n",
                user("first"),
                user("last")
            ));
            let store = store();
            let (owner, cap) = admission_owner(background);
            let root = finish(
                &store,
                store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                &owner,
            );
            let prepare = prepare_request(&root);
            let template = acquire(Path::new("/unused"));
            let slice_query = |prepared: &Value| json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":prepared["data"]["handle"],"selectors":[{"kind":"current_turn"}],"query":{"kind":"has_read","pattern":"root.rs","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let slices = |prepared: &Value| {
                store.lock_state().prepared_graphs
                    [prepared["data"]["handle"]["graph_id"].as_str().unwrap()]
                .lock()
                .unwrap()
                .root_slices
                .len()
            };
            let measured = finish(
                &store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            let before_slice = settled_bytes(&store);
            let first = store.request(&slice_query(&measured), &owner, &Cancellation::default());
            assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
            let slice_bytes = settled_bytes(&store) - before_slice;
            assert!(slice_bytes > 0);
            assert_eq!(slices(&measured), 1);
            let prepared = finish(
                &store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(slices(&prepared), 0);
            let idle = store.retained_accounted_bytes();
            let capacities = idle - settled_bytes(&store);
            let room = cap - idle - slice_bytes - REPLY_RESERVATION;
            let crowded = store.reserve_projection(&owner, room + 1).unwrap();
            let refused = store.request(&slice_query(&prepared), &owner, &Cancellation::default());
            assert_refused(&store, &refused);
            assert_eq!(store.retained_accounted_bytes(), idle + room + 1);
            assert_eq!(slices(&prepared), 0);
            drop(crowded);
            let fitted = store.reserve_projection(&owner, room).unwrap();
            let exact = store.request(&slice_query(&prepared), &owner, &Cancellation::default());
            assert_eq!(exact["status"].as_str(), Some("ok"), "{exact:?}");
            assert_eq!(settled_bytes(&store), cap - REPLY_RESERVATION - capacities);
            assert_eq!(slices(&prepared), 1);
            store.assert_conserved();
            drop(fitted);
        }
    }

    #[test]
    fn reregistering_an_interned_registry_reserves_nothing() {
        let store = NativeStore::new(
            &json!({"max_retained_bytes":8*1024*1024,"reserved_hook_accounted_bytes":8*1024*1024}),
        )
        .unwrap();
        let hook = context("owner");
        let mut review = hook.clone();
        review.insert("admission", json!("review"));
        let other_uid = json!({"claimant":"owner","admission":"hook","authority":{"kind":"user","effective_uid":"0"},"registry_generation":hook["registry_generation"]});
        let specs = json!([{"name":"fetch","behaves_like":"Read","span_edit":null}]);
        let reordered = json!([{"span_edit":null,"behaves_like":"Read","name":"fetch"}]);
        let fingerprint = store.register_tool_registry(&specs, &hook).unwrap();
        let registered = store.state.lock().unwrap().registries.len();
        let first = Arc::clone(&store.state.lock().unwrap().registries[&fingerprint].snapshot);
        assert_eq!(
            store.register_tool_registry(&reordered, &review).unwrap(),
            fingerprint
        );
        assert_eq!(
            store
                .register_tool_registry(&specs, &other_uid)
                .unwrap_err()
                .status,
            Status::PermissionDenied
        );
        assert_eq!(
            store
                .register_tool_registry(
                    &json!([{"name":"","behaves_like":"Read","span_edit":null}]),
                    &hook
                )
                .unwrap_err()
                .status,
            Status::InvalidRequest
        );
        let state = store.state.lock().unwrap();
        assert_eq!(state.registries.len(), registered);
        assert!(Arc::ptr_eq(
            &first,
            &state.registries[&fingerprint].snapshot
        ));
        assert_eq!(state.transient_bytes, 0);
    }
}

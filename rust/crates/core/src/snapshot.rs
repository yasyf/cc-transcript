use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::ops::Range;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use sonic_rs::JsonType;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::gateway::{sniff_provider, Provider};
use crate::snapshot_activity::ActivityIndex;
use crate::snapshot_ledger::{
    arc_bytes, arc_slice_bytes, charged_bytes, deque_capacity_for, deque_growth, hashbrown_tier,
    map_capacity_for, map_growth, set_capacity_for, set_growth, vec_capacity_for, vec_growth,
    Anchor, Charge, DeadlineIndex, ExpiryIndex, LedgerEvent, LedgerHook, Ledgered, RetainedLedger,
    Table, TicketKey, Work, MUTEX_STORAGE_BYTES,
};
#[cfg(test)]
use crate::snapshot_ledger::{arc_mirror, arc_slice_mirror, Reserved, MUTEX_STORAGE_MIRROR};
use crate::snapshot_memory::{
    arena_bytes, arena_charge, dom_parse_bound, entry_charge, MemoryCharge, SourceArena,
};
use crate::snapshot_prepared::FactsNeed;
use crate::snapshot_prepared_disk::PreparedDiskKey;
use crate::snapshot_projection::JSON_LITERAL_OBJECT_CAPACITY;
use crate::types::Entry;
#[cfg(test)]
use crate::types::SYNTHETIC_MODEL;

pub const SCHEMA: &str = "cc-transcript.snapshot/1";
pub const PARSER_VERSION: &str = "cc-transcript.snapshot/1";
pub const MAX_REPLY_BYTES: usize = 1_044_480;
const MAX_DATA_BYTES: usize = MAX_REPLY_BYTES - 2048;
const FILESYSTEM_PATH_BYTES: usize = 2 * (libc::PATH_MAX as usize + size_of::<libc::dirent>());
const LOCATE_PATH_SLOTS: usize = 2 * FILESYSTEM_PATH_BYTES;
const LOCATE_USER_PATH_SLOTS: usize = 3 * FILESYSTEM_PATH_BYTES;
const READ_DIR_HANDLE_BYTES: usize = arc_bytes::<OpenedDirectory>();
const LOAD_SLOT_BYTES: usize = arc_bytes::<LoadSlot>() + MUTEX_STORAGE_BYTES;
const CLASSIFIER_SLOT_BYTES: usize = arc_bytes::<ClassifierSlot>() + MUTEX_STORAGE_BYTES;
const PREPARED_GRAPH_BYTES: usize = arc_bytes::<Mutex<PreparedGraph>>() + MUTEX_STORAGE_BYTES;
const RELEASE_QUEUE_BYTES: usize =
    arc_bytes::<crate::snapshot_ledger::ReleaseQueue>() + MUTEX_STORAGE_BYTES;

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

enum LineDecode {
    Deferred,
    Decoded {
        entry: Option<Entry>,
        arenas: Vec<SourceArena>,
        charge: usize,
    },
}

#[derive(Debug, Default, Clone)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn shares(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
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

    pub fn window_bytes(&self) -> u64 {
        self.size - self.identity.window_base
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
    arenas: Vec<(usize, SourceArena)>,
}

impl ChunkRows {
    pub fn new(rows: Vec<Entry>) -> Self {
        Self::retaining(rows, Vec::new())
    }

    fn retaining(rows: Vec<Entry>, arenas: Vec<(usize, SourceArena)>) -> Self {
        Self {
            ledger: LedgerHook::default(),
            rows,
            arenas,
        }
    }

    pub fn arenas(&self) -> &Vec<(usize, SourceArena)> {
        &self.arenas
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
    ledger: LedgerHook,
    pub entries: Arc<ChunkRows>,
    pub start: usize,
    pub charge: MemoryCharge,
    pub entry_charges: Vec<MemoryCharge>,
    pub user_count: usize,
    pub sidechain_user_count: usize,
}

impl EntryChunk {
    pub fn new(start: usize, entries: Vec<Entry>) -> Self {
        Self::retaining(start, entries, Vec::new())
    }

    fn metadata_bytes(&self) -> usize {
        arc_bytes::<Self>() + self.entry_charges.capacity() * size_of::<MemoryCharge>()
    }

    fn rows_charge(&self) -> MemoryCharge {
        MemoryCharge {
            owned_capacity_bytes: self.charge.owned_capacity_bytes - self.metadata_bytes(),
            opaque_dom_accounted_bytes: self.charge.opaque_dom_accounted_bytes,
        }
    }

    fn rebased(&self, start: usize) -> Self {
        let entry_charges = self.entry_charges.clone();
        Self {
            ledger: LedgerHook::default(),
            entries: Arc::clone(&self.entries),
            start,
            charge: MemoryCharge {
                owned_capacity_bytes: self.rows_charge().owned_capacity_bytes
                    + arc_bytes::<Self>()
                    + entry_charges.capacity() * size_of::<MemoryCharge>(),
                opaque_dom_accounted_bytes: self.charge.opaque_dom_accounted_bytes,
            },
            entry_charges,
            user_count: self.user_count,
            sidechain_user_count: self.sidechain_user_count,
        }
    }

    pub(crate) fn retaining(
        start: usize,
        entries: Vec<Entry>,
        arenas: Vec<(usize, SourceArena)>,
    ) -> Self {
        let mut charge = MemoryCharge {
            owned_capacity_bytes: arc_bytes::<Self>()
                + arc_bytes::<ChunkRows>()
                + entries.capacity() * size_of::<Entry>()
                + arenas.capacity() * size_of::<(usize, SourceArena)>(),
            opaque_dom_accounted_bytes: 0,
        };
        let mut entry_charges: Vec<_> = entries.iter().map(entry_charge).collect();
        for (entry, arena) in &arenas {
            entry_charges[*entry] += arena_charge(arena);
        }
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
            ledger: LedgerHook::default(),
            entries: Arc::new(ChunkRows::retaining(entries, arenas)),
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

    pub fn window_started_unix_ms(&self) -> Option<i64> {
        if self.window_start == 0 {
            return None;
        }
        self.activity.started_unix_ms()
    }

    pub fn model(&self) -> Option<&str> {
        self.activity
            .last_model_event()
            .map(|position| match self.entry(position) {
                Entry::Assistant(assistant) => assistant.model.as_str(),
                _ => unreachable!("indexed model is an assistant event"),
            })
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
                owned_capacity_bytes: arc_bytes::<Self>()
                    + self.id.capacity()
                    + self.canonical_path.capacity()
                    + self.session_id.capacity()
                    + self.chunks.capacity() * size_of::<Arc<EntryChunk>>()
                    + self.fence.capacity(),
                opaque_dom_accounted_bytes: 0,
            },
        )];
        entries.extend(self.chunks.iter().flat_map(|chunk| {
            [
                (
                    Arc::as_ptr(chunk) as usize,
                    MemoryCharge {
                        owned_capacity_bytes: chunk.metadata_bytes(),
                        opaque_dom_accounted_bytes: 0,
                    },
                ),
                (Arc::as_ptr(&chunk.entries) as usize, chunk.rows_charge()),
            ]
        }));
        if let Some(raw) = &self.codex_raw {
            entries.push((
                Arc::as_ptr(raw) as usize,
                MemoryCharge {
                    owned_capacity_bytes: arc_bytes::<Vec<u8>>() + raw.capacity(),
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

const COUNTERS: [&str; 20] = [
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
    "prepared_cache_reads",
    "prepared_cache_bytes_read",
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
    scan_read: usize,
    scan_events: usize,
    scan_discovery: usize,
    scan_sources: usize,
    scan_validate: usize,
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
    delivery: usize,
    registry_generation: String,
    registry: Arc<crate::toolcall::ToolRegistrySnapshot>,
}

struct LeasedScope {
    snapshot: Arc<TranscriptSnapshot>,
    classifier: Value,
    expires: u64,
}

// Sized exactly like an acquire reply's re-serialized copy, whose bytes admit graph members.
#[derive(serde::Serialize)]
struct LeaseHandle<'a> {
    owner_epoch: &'a str,
    snapshot_id: &'a str,
    generation: &'a str,
    lease_id: &'a str,
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
    fn record_bytes(&self) -> usize {
        LOAD_SLOT_BYTES
            + self.id.capacity()
            + self.path.capacity()
            + self.registry_generation.capacity()
    }

    fn ledgered_bytes(&self) -> usize {
        if self.attached.load(Ordering::Acquire) {
            self.accounted.load(Ordering::Acquire)
        } else {
            0
        }
    }
}

enum ChunkAllocationOwner {
    Rows(Weak<ChunkRows>),
    Metadata(Weak<EntryChunk>),
}

impl ChunkAllocationOwner {
    fn strong_count(&self) -> usize {
        match self {
            Self::Rows(rows) => rows.strong_count(),
            Self::Metadata(chunk) => chunk.strong_count(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SourceChunkSpan {
    start: u64,
    end: u64,
}

struct Load {
    charge_work: Work,
    fresh_chunk_bytes: usize,
    activity_from_previous: bool,
    codex_append_bytes: usize,
    file: File,
    offset: u64,
    pending: Vec<u8>,
    pending_start: u64,
    provider: Option<Provider>,
    chunks: Vec<Arc<EntryChunk>>,
    source_chunks: Vec<SourceChunkSpan>,
    previous_chunks: Option<Arc<Vec<SourceChunkSpan>>>,
    published_chunks: Option<Arc<Vec<SourceChunkSpan>>>,
    reuse_next: Option<usize>,
    reuse_prepared: bool,
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
    directories: Vec<OpenDirectory>,
    seen: HashSet<SourceIdentity>,
    seen_directories: HashSet<SourceIdentity>,
    examined: usize,
    sources: usize,
    emitted: usize,
    output_bytes: usize,
    inventory: HashMap<String, Value>,
    inventory_payload: usize,
    previous: HashMap<String, Value>,
    removed: Vec<Value>,
    walking: bool,
    expires: u64,
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
    directories: Vec<OpenDirectory>,
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
            + self.scope.iter().map(PathBuf::capacity).sum::<usize>()
            + self.roots.capacity() * size_of::<PathBuf>()
            + self.roots.iter().map(PathBuf::capacity).sum::<usize>()
            + self.directories.capacity() * size_of::<OpenDirectory>()
            + self
                .directories
                .iter()
                .map(OpenDirectory::retained_bytes)
                .sum::<usize>()
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

fn located_item_bytes(id: &str, status: &str, located: Option<(&Path, &str)>) -> usize {
    let pair = size_of::<(Value, Value)>();
    hashbrown_tier(JSON_LITERAL_OBJECT_CAPACITY, pair) * pair
        + ["session_id", "status", "path", "revision"]
            .iter()
            .map(|key| key.len())
            .sum::<usize>()
        + id.len()
        + status.len()
        + located.map_or(0, |(path, revision)| {
            path.to_string_lossy().len() + revision.len()
        })
}

fn inventory_bytes(inventory: &HashMap<String, Value>) -> usize {
    inventory.capacity() * size_of::<(String, Value)>()
        + inventory
            .iter()
            .map(|(path, value)| path.capacity() + value_bytes(value))
            .sum::<usize>()
}

impl DiscoveryCursor {
    fn accounted_bytes(&self) -> usize {
        self.claimant.capacity()
            + value_bytes(&self.request)
            + value_bytes(&self.context)
            + self.roots.capacity() * size_of::<PathBuf>()
            + self.roots.iter().map(PathBuf::capacity).sum::<usize>()
            + self.directories.capacity() * size_of::<OpenDirectory>()
            + self
                .directories
                .iter()
                .map(OpenDirectory::retained_bytes)
                .sum::<usize>()
            + self.seen.capacity() * size_of::<SourceIdentity>()
            + self.seen_directories.capacity() * size_of::<SourceIdentity>()
            + self.inventory.capacity() * size_of::<(String, Value)>()
            + self.inventory_payload
            + inventory_bytes(&self.previous)
            + self.removed.capacity() * size_of::<Value>()
            + self.removed.iter().map(value_bytes).sum::<usize>()
    }
}

struct Checkpoint {
    claimant: String,
    roots: Value,
    inventory: HashMap<String, Value>,
    expires: u64,
}

impl Checkpoint {
    fn accounted_bytes(&self) -> usize {
        self.claimant.capacity() + value_bytes(&self.roots) + inventory_bytes(&self.inventory)
    }
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

impl CachedPreparedFacts {
    fn serves(
        &self,
        stamp: SourceStamp,
        registry_generation: &str,
        admission: &str,
        authority: &Value,
        classifier: &Value,
        need: FactsNeed,
    ) -> bool {
        self.stamp == stamp
            && self.registry_generation == registry_generation
            && self.admission == admission
            && self.authority == *authority
            && self.classifier == *classifier
            && self.facts.serves(need)
    }

    fn evictable(&self) -> bool {
        Arc::strong_count(&self.facts) == 1
    }
}

enum PreparedSourceOutcome<'a> {
    Ready {
        stamp: SourceStamp,
        held: HeldFacts<'a>,
        cached: bool,
    },
    Pending(String),
    Stored,
    Superseded,
}

enum HeldFacts<'a> {
    Retained(RetainedFacts<'a>),
    Reserved {
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        reservation: ProjectionReservation<'a>,
    },
}

impl HeldFacts<'_> {
    fn facts(&self) -> &Arc<crate::snapshot_prepared::PreparedFacts> {
        match self {
            Self::Retained(retained) => retained.facts(),
            Self::Reserved { facts, .. } => facts,
        }
    }
}

#[derive(Clone, Copy)]
enum PreparedCacheReads<'a> {
    Unmetered,
    Within(usize),
    Warming(usize, &'a WarmMembership, Option<i128>),
}

impl PreparedCacheReads<'_> {
    fn admits(self, bytes: usize) -> bool {
        match self {
            Self::Unmetered => true,
            Self::Within(allowance) | Self::Warming(allowance, ..) => bytes <= allowance,
        }
    }
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
    catalog: Option<CatalogCertificate>,
    members: Arc<[PreparedSourceRef]>,
    positions: Arc<[(SourceIdentity, usize)]>,
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
            positions_anchor(&self.positions),
            sidechain_dirs_anchor(&self.sidechain_dirs),
        ]
        .into_iter()
    }

    fn active_member(
        &self,
        identity: SourceIdentity,
        since: Option<i128>,
    ) -> Option<&PreparedSourceRef> {
        self.positions
            .binary_search_by_key(&identity, |(member, _)| *member)
            .ok()
            .map(|found| &self.members[self.positions[found].1])
            .filter(|member| active(member, since))
    }
}

fn member_positions(members: &[PreparedSourceRef]) -> Arc<[(SourceIdentity, usize)]> {
    let mut positions: Arc<[(SourceIdentity, usize)]> = members
        .iter()
        .enumerate()
        .map(|(position, member)| (member.stamp.identity, position))
        .collect();
    Arc::get_mut(&mut positions)
        .expect("unshared member positions")
        .sort_unstable();
    positions
}

fn slice_key<T>(buffer: &Arc<[T]>) -> usize {
    Arc::as_ptr(buffer) as *const T as usize
}

fn sources_anchor(sources: &Arc<[PreparedSourceRef]>) -> Anchor {
    Anchor::warm(slice_key(sources), source_ref_bytes(sources))
}

fn positions_anchor(positions: &Arc<[(SourceIdentity, usize)]>) -> Anchor {
    Anchor::warm(
        slice_key(positions),
        arc_slice_bytes::<(SourceIdentity, usize)>(positions.len()),
    )
}

fn sidechain_dirs_anchor(sidechain_dirs: &Arc<[(PathBuf, Option<SourceStamp>)]>) -> Anchor {
    Anchor::warm(
        slice_key(sidechain_dirs),
        sidechain_dir_bytes(sidechain_dirs),
    )
}

fn source_ref_bytes(sources: &Arc<[PreparedSourceRef]>) -> usize {
    arc_slice_bytes::<PreparedSourceRef>(sources.len())
        + sources
            .iter()
            .map(|source| source.path.capacity())
            .sum::<usize>()
}

fn sidechain_dir_bytes(sidechain_dirs: &Arc<[(PathBuf, Option<SourceStamp>)]>) -> usize {
    arc_slice_bytes::<(PathBuf, Option<SourceStamp>)>(sidechain_dirs.len())
        + sidechain_dirs
            .iter()
            .map(|(path, _)| path.capacity())
            .sum::<usize>()
}

fn stamp_bytes(stamps: &Vec<(PathBuf, SourceStamp)>) -> usize {
    stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
        + stamps
            .iter()
            .map(|(path, _)| path.capacity())
            .sum::<usize>()
}

fn task_bytes(task: &GraphTask) -> usize {
    match task {
        GraphTask::Visit {
            path, spawned_by, ..
        } => path.capacity() + spawned_by.as_ref().map_or(0, String::capacity),
        GraphTask::List { parent, .. } => parent.capacity(),
    }
}

fn spawner(path: &Path) -> &str {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    stem.strip_prefix("agent-").unwrap_or(stem)
}

fn graph_spawned_by(path: &Path) -> Cow<'_, str> {
    let stem = path.file_stem().expect("sidechain filename");
    match stem.to_string_lossy() {
        Cow::Borrowed(stem) => Cow::Borrowed(stem.strip_prefix("agent-").unwrap_or(stem)),
        Cow::Owned(stem) => Cow::Owned(stem.strip_prefix("agent-").unwrap_or_default().to_owned()),
    }
}

fn sidechain_directory_capacity(base: &Path, stem: &OsStr) -> usize {
    base.as_os_str().len() + 1 + stem.len() + 1 + "subagents".len()
}

fn sidechain_directory(base: &Path, stem: &OsStr) -> PathBuf {
    let mut directory = PathBuf::with_capacity(sidechain_directory_capacity(base, stem));
    directory.push(base);
    directory.push(stem);
    directory.push("subagents");
    directory
}

fn realpath(path: &Path) -> std::io::Result<PathBuf> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file name contained an unexpected NUL byte",
        ));
    }
    if bytes.len() >= libc::PATH_MAX as usize {
        return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    let mut input = [0u8; libc::PATH_MAX as usize];
    input[..bytes.len()].copy_from_slice(bytes);
    let mut resolved = [0u8; libc::PATH_MAX as usize];
    if unsafe { libc::realpath(input.as_ptr().cast(), resolved.as_mut_ptr().cast()) }.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let len = resolved
        .iter()
        .position(|byte| *byte == 0)
        .expect("realpath terminates its result");
    Ok(PathBuf::from(OsString::from_vec(resolved[..len].to_vec())))
}

struct PreparedBuild {
    charge_work: Work,
    owned_payload_bytes: usize,
    context_bytes: usize,
    claimant: String,
    context: Value,
    request: Value,
    root: Arc<TranscriptSnapshot>,
    root_handle: Value,
    classifier: Value,
    root_facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    remaining: WorkLimits,
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
    observers: Observers,
    need: FactsNeed,
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
    root_slices: Table<String, Arc<crate::snapshot_prepared::PreparedFacts>>,
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
    entries: OpenDirectory,
    children: Vec<PathBuf>,
    depth: usize,
}

struct OpenedDirectory {
    directory: std::ptr::NonNull<libc::DIR>,
    root: PathBuf,
    stamp: SourceStamp,
    #[cfg(test)]
    parent_metadata_attempts: AtomicUsize,
}

// Only OpenDirectory owns iteration. Shared entry metadata operations do not mutate iteration state.
unsafe impl Send for OpenedDirectory {}
unsafe impl Sync for OpenedDirectory {}

impl Drop for OpenedDirectory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.directory.as_ptr());
        }
    }
}

struct OpenDirectory {
    entries: Arc<OpenedDirectory>,
    end_of_stream: bool,
}

struct OpenDirectoryEntry {
    directory: Arc<OpenedDirectory>,
    name: OsString,
    kind: u8,
    inode: u64,
}

impl OpenDirectoryEntry {
    fn path(&self) -> PathBuf {
        self.directory.root.join(&self.name)
    }
    fn file_name(&self) -> OsString {
        self.name.clone()
    }

    fn name_evidence(&self, _store: &NativeStore) -> CatalogNameEvidence {
        let mut kind = (self.kind != libc::DT_UNKNOWN).then_some(self.kind);
        #[cfg(test)]
        if _store.directory_force_unknown.load(Ordering::Relaxed) {
            kind = None;
        }
        CatalogNameEvidence {
            inode: self.inode,
            kind,
        }
    }

    fn symlink_hint(&self, store: &NativeStore) -> Option<bool> {
        self.name_evidence(store)
            .kind
            .map(|kind| kind == libc::DT_LNK)
    }
}

impl OpenedDirectory {
    fn current_stamp(&self) -> std::io::Result<SourceStamp> {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        #[cfg(test)]
        self.parent_metadata_attempts
            .fetch_add(1, Ordering::Relaxed);
        if unsafe { libc::fstat(libc::dirfd(self.directory.as_ptr()), metadata.as_mut_ptr()) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(CatalogChildMetadata::from_descriptor(&unsafe { metadata.assume_init() }).stamp)
    }

    fn object_metadata(
        &self,
        name: &OsStr,
        _store: &NativeStore,
    ) -> std::io::Result<CatalogChildMetadata> {
        let name = std::ffi::CString::new(name.as_encoded_bytes())
            .expect("directory name contains no NUL");
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        #[cfg(test)]
        {
            _store
                .directory_descriptor_metadata
                .fetch_add(1, Ordering::Relaxed);
            _store.after_location_metadata("descriptor_metadata");
        }
        let result = unsafe {
            libc::fstatat(
                libc::dirfd(self.directory.as_ptr()),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let metadata = unsafe { metadata.assume_init() };
        Ok(CatalogChildMetadata::from_descriptor(&metadata))
    }
}

impl OpenDirectory {
    fn open(path: &Path) -> Result<Self, SnapshotError> {
        let encoded =
            std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
                io_error(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file name contained an unexpected NUL byte",
                ))
            })?;
        let directory = std::ptr::NonNull::new(unsafe { libc::opendir(encoded.as_ptr()) })
            .ok_or_else(|| io_error(std::io::Error::last_os_error()))?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(libc::dirfd(directory.as_ptr()), metadata.as_mut_ptr()) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::closedir(directory.as_ptr());
            }
            return Err(io_error(error));
        }
        let metadata = unsafe { metadata.assume_init() };
        let stamp = CatalogChildMetadata::from_descriptor(&metadata).stamp;
        Ok(Self {
            entries: Arc::new(OpenedDirectory {
                directory,
                root: path.to_owned(),
                stamp,
                #[cfg(test)]
                parent_metadata_attempts: AtomicUsize::new(0),
            }),
            end_of_stream: false,
        })
    }

    fn stamp(&self) -> SourceStamp {
        self.entries.stamp
    }

    fn retained_bytes(&self) -> usize {
        READ_DIR_HANDLE_BYTES + self.entries.root.capacity()
    }
}

fn directory_errno() -> *mut libc::c_int {
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    unsafe {
        libc::__error()
    }
    #[cfg(any(target_os = "android", target_os = "openbsd", target_os = "netbsd"))]
    unsafe {
        libc::__errno()
    }
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "android",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    unsafe {
        libc::__errno_location()
    }
}

impl Iterator for OpenDirectory {
    type Item = std::io::Result<OpenDirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.end_of_stream {
            return None;
        }
        loop {
            let entry = unsafe {
                *directory_errno() = 0;
                libc::readdir(self.entries.directory.as_ptr())
            };
            if entry.is_null() {
                self.end_of_stream = true;
                let error = std::io::Error::last_os_error();
                return (error.raw_os_error() != Some(0)).then_some(Err(error));
            }
            // dirent may have a dynamically sized trailing name; never form a reference to its full layout.
            let name =
                unsafe { std::ffi::CStr::from_ptr(std::ptr::addr_of!((*entry).d_name).cast()) }
                    .to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            return Some(Ok(OpenDirectoryEntry {
                directory: Arc::clone(&self.entries),
                name: OsString::from_vec(name.to_vec()),
                kind: unsafe { (*entry).d_type },
                inode: unsafe { (*entry).d_ino as u64 },
            }));
        }
    }
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
}

impl GraphCursor {
    fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.claimant.capacity()
            + value_bytes(&self.context)
            + value_bytes(&self.request)
            + value_bytes(&self.root_handle)
            + self.nodes.capacity() * size_of::<GraphNode>()
            + self
                .nodes
                .iter()
                .map(|node| {
                    node.path.capacity()
                        + node.spawned_by.as_ref().map_or(0, String::capacity)
                        + value_bytes(&node.description)
                })
                .sum::<usize>()
            + self.seen.capacity() * size_of::<SourceIdentity>()
            + self.tasks.capacity() * size_of::<GraphTask>()
            + self
                .tasks
                .iter()
                .map(|task| match task {
                    GraphTask::Visit {
                        path, spawned_by, ..
                    } => path.capacity() + spawned_by.as_ref().map_or(0, String::capacity),
                    GraphTask::List { parent, .. } => parent.capacity(),
                })
                .sum::<usize>()
            + self.listing.as_ref().map_or(0, |listing| {
                listing.children.capacity() * size_of::<PathBuf>()
                    + listing
                        .children
                        .iter()
                        .map(PathBuf::capacity)
                        .sum::<usize>()
                    + listing.entries.retained_bytes()
            })
            + self.pending.as_ref().map_or(0, |pending| {
                pending.token.capacity()
                    + pending.path.capacity()
                    + pending.spawned_by.as_ref().map_or(0, String::capacity)
            })
            + self.pending_records.capacity() * size_of::<(usize, String)>()
            + self
                .pending_records
                .iter()
                .map(|(_, record)| record.capacity())
                .sum::<usize>()
            + self.published_members.capacity() * size_of::<usize>()
    }
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
            self.activity.heap_allocations().chain(
                self.committed
                    .iter()
                    .flat_map(ActivityIndex::heap_allocations),
            ),
            &self.carried,
        ) + self.carried.capacity() * size_of::<usize>()
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
        std::iter::once(Anchor::indexes((
            self as *const Self as usize,
            arc_bytes::<Self>() + self.prefix.capacity() * size_of::<Arc<EntryChunk>>(),
        )))
        .chain(
            self.activity
                .shared_allocations()
                .into_iter()
                .map(Anchor::indexes),
        )
    }
}

impl Charge<(SourceIdentity, String)> for Arc<CarriedClassification> {
    fn key_charge((_, lineage): &(SourceIdentity, String)) -> usize {
        lineage.capacity()
    }

    fn charge(&self) -> usize {
        0
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
    chunks: Vec<Arc<EntryChunk>>,
    accounted: AtomicUsize,
    attached: AtomicBool,
    deadline: u64,
    complete: AtomicBool,
}

impl ClassifierSlot {
    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.seed.iter().flat_map(|seed| seed.anchors())
    }

    fn record_bytes(&self) -> usize {
        CLASSIFIER_SLOT_BYTES + self.chunks.capacity() * size_of::<Arc<EntryChunk>>()
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
    cached: Option<CachedSnapshot>,
    registry_generation: String,
    entries: Vec<(usize, MemoryCharge)>,
    indexes: Vec<(usize, usize)>,
    source_chunks: Option<Arc<Vec<SourceChunkSpan>>>,
    raw_window: Option<Arc<crate::snapshot_raw_window::RawWindow>>,
    raw_parent: Option<Arc<TranscriptSnapshot>>,
    raw_cache: Option<Box<RawCapabilityCache>>,
    interactions: Option<Arc<InteractionIndex>>,
}

impl GenerationRecord {
    fn new(snapshot: &Arc<TranscriptSnapshot>, registry_generation: &str) -> Self {
        Self {
            snapshot: Arc::downgrade(snapshot),
            cached: None,
            registry_generation: registry_generation.to_owned(),
            entries: snapshot.accounted_allocations(),
            indexes: snapshot.activity.shared_allocations(),
            source_chunks: None,
            raw_window: None,
            raw_parent: None,
            raw_cache: None,
            interactions: None,
        }
    }

    fn anchors(&self) -> impl Iterator<Item = Anchor> + '_ {
        self.entries
            .iter()
            .map(|(id, charge)| Anchor::entries(*id, *charge))
            .chain(self.indexes.iter().copied().map(Anchor::indexes))
            .chain(self.raw_window.iter().flat_map(raw_window_anchors))
    }
}

impl Charge<usize> for GenerationRecord {
    fn charge(&self) -> usize {
        self.source_chunks.as_ref().map_or(0, |chunks| {
            arc_bytes::<Vec<SourceChunkSpan>>() + chunks.capacity() * size_of::<SourceChunkSpan>()
        }) + self.registry_generation.capacity()
            + self.cached.as_ref().map_or(0, |cached| {
                cached.classified.as_ref().map_or(0, String::capacity)
            })
            + self.entries.capacity() * size_of::<(usize, MemoryCharge)>()
            + self.indexes.capacity() * size_of::<(usize, usize)>()
            + self
                .raw_cache
                .as_ref()
                .map_or(0, |cache| cache.metadata_bytes())
            + self
                .interactions
                .as_ref()
                .map_or(0, |index| index.accounted_bytes)
    }
}

struct CachedSnapshot {
    snapshot: Arc<TranscriptSnapshot>,
    latest: Option<SourceIdentity>,
    spine: Option<SourceIdentity>,
    classified: Option<String>,
    touched: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CacheOwner {
    Generation(usize),
    Carried(usize),
    Catalog(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceView {
    window_start: u64,
    size: std::cmp::Reverse<u64>,
    generation: usize,
}

impl SourceView {
    fn of(snapshot: &Arc<TranscriptSnapshot>) -> Self {
        Self {
            window_start: snapshot.window_start,
            size: std::cmp::Reverse(snapshot.stamp.size),
            generation: snapshot_key(snapshot),
        }
    }
}

struct CarriedOwner {
    lineages: Vec<(SourceIdentity, String)>,
    touched: u64,
}

struct CatalogOwner {
    key: Arc<str>,
    touched: u64,
}

impl Charge<usize> for CatalogOwner {
    fn charge(&self) -> usize {
        arc_slice_bytes::<u8>(self.key.len())
    }
}

impl Charge<usize> for CarriedOwner {
    fn charge(&self) -> usize {
        self.lineages.capacity() * size_of::<(SourceIdentity, String)>()
            + self
                .lineages
                .iter()
                .map(|(_, key)| key.capacity())
                .sum::<usize>()
    }
}

impl Charge<String> for usize {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        0
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

fn entry_bytes(snapshot: &TranscriptSnapshot) -> usize {
    snapshot
        .chunks
        .iter()
        .map(|chunk| chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes)
        .sum()
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
        self.claimant.capacity()
            + self.registry_generation.capacity()
            + value_bytes(&self.classifier)
            + self.delivery
    }
}

impl Lease {
    fn pledge(token: &String, context: &Value, classifier: &Value) -> Result<usize, SnapshotError> {
        let claimant = str_field(context, "claimant")?;
        Ok(Self::key_charge(token)
            + claimant.len()
            + str_field(context, "registry_generation")?.len()
            + value_bytes(classifier)
            + Delivery::lease_pledge(claimant, token))
    }
}

impl Charge<String> for Waiter {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.claimant.capacity() + value_bytes(&self.context) + value_bytes(&self.classifier)
    }
}

impl Charge<String> for ProjectionCursor {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.claimant.capacity()
            + self.registry_generation.capacity()
            + self.admission.capacity()
            + value_bytes(&self.request)
    }
}

impl Charge<String> for DiscoveryCursor {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.accounted_bytes()
    }
}

impl Charge<String> for Checkpoint {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.accounted_bytes()
    }
}

impl Charge<String> for ResolutionCursor {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.claimant.capacity()
            + value_bytes(&self.context)
            + value_bytes(&self.request)
            + self.ids.capacity() * size_of::<String>()
            + self.ids.iter().map(String::capacity).sum::<usize>()
            + self.paths.capacity() * size_of::<(String, PathBuf)>()
            + self
                .paths
                .iter()
                .map(|(id, path)| id.capacity() + path.capacity())
                .sum::<usize>()
            + self.sessions.capacity() * size_of::<Value>()
            + self.sessions.iter().map(value_bytes).sum::<usize>()
            + self.pending.as_ref().map_or(0, String::capacity)
    }
}

impl Charge<String> for LocatedPath {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.path.capacity()
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
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.accounted_bytes()
    }
}

impl Charge<String> for PreparedGraph {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        PREPARED_GRAPH_BYTES
            + self.claimant.capacity()
            + self.registry_generation.capacity()
            + self.admission.capacity()
            + self.revision.capacity()
            + value_bytes(&self.authority)
            + value_bytes(&self.root_handle)
            + value_bytes(&self.classifier)
            + stamp_bytes(&self.stamps)
            + self.root_slices.reserved_bytes()
            + self.root_slices.keys().map(String::capacity).sum::<usize>()
    }
}

impl Charge<String> for Arc<Mutex<PreparedGraph>> {
    fn key_charge(key: &String) -> usize {
        PreparedGraph::key_charge(key)
    }

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
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        self.charge_work.tick(1);
        size_of::<PreparedBuild>()
            + self.owned_payload_bytes
            + self.tasks.capacity() * size_of::<GraphTask>()
            + self.listing.as_ref().map_or(0, |listing| {
                listing.children.capacity() * size_of::<PathBuf>()
                    + listing.entries.retained_bytes()
            })
            + self.seen.capacity() * size_of::<SourceIdentity>()
            + self.sources.capacity() * size_of::<PreparedSourceRef>()
            + self.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
            + self.sidechain_dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
    }
}

impl Charge<String> for PreparedQueryCursor {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        size_of::<PreparedQueryCursor>()
            + self.claimant.capacity()
            + self.graph_id.capacity()
            + value_bytes(&self.query)
            + self.pending.as_ref().map_or(0, |pending| {
                pending.token.capacity() + pending.path.capacity()
            })
            + self.input_records.as_ref().map_or(0, |records| {
                records.capacity() * size_of::<String>()
                    + records.iter().map(String::capacity).sum::<usize>()
            })
            + self.observers.charge()
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

impl Charge<PreparedDiskKey> for CachedPreparedFacts {
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
        arc_slice_bytes::<u8>(key.len())
    }

    fn charge(&self) -> usize {
        size_of::<Delivery>()
            + self.claimant.capacity()
            + self.leases.capacity() * size_of::<String>()
            + self.leases.iter().map(String::capacity).sum::<usize>()
            + self.cursor.as_ref().map_or(0, String::capacity)
    }
}

impl Delivery {
    fn record_bytes(key: &Arc<str>, delivery: &Self) -> usize {
        charged_bytes(key, delivery) + size_of::<(u64, Arc<str>)>()
    }

    fn cursor_pledge(claimant: &str, cursor: &str) -> usize {
        size_of::<Self>()
            + claimant.len()
            + cursor.len()
            + arc_slice_bytes::<u8>(2 * <Sha256 as Digest>::output_size())
            + size_of::<(u64, Arc<str>)>()
    }

    fn lease_pledge(claimant: &str, lease: &str) -> usize {
        Self::cursor_pledge(claimant, lease) + size_of::<String>()
    }
}

pub(crate) struct StoreState {
    gauge_reports: Work,
    latest: Table<SourceIdentity, usize>,
    source_spines: Table<SourceIdentity, BTreeSet<SourceView>>,
    source_view_nodes: usize,
    source_view_checks: Work,
    classified: Ledgered<String, usize>,
    cache_order: BTreeSet<(u64, CacheOwner)>,
    cache_clock: u64,
    carried_owners: Ledgered<usize, CarriedOwner>,
    catalog_owners: Ledgered<usize, CatalogOwner>,
    recent_codex: Table<SourceIdentity, u64>,
    loads: Table<SourceIdentity, Arc<LoadSlot>>,
    prepared_loads: Table<SourceIdentity, (Arc<LoadSlot>, u64)>,
    leases: Ledgered<String, Lease>,
    waiters: Ledgered<String, Waiter>,
    projections: Ledgered<String, ProjectionCursor>,
    generations: Ledgered<usize, GenerationRecord>,
    classifier_stages: Table<String, Arc<ClassifierSlot>>,
    carried_classifications: Ledgered<(SourceIdentity, String), Arc<CarriedClassification>>,
    graphs: Ledgered<String, GraphCursor>,
    prepared_graphs: Ledgered<String, Arc<Mutex<PreparedGraph>>>,
    prepared_builds: Ledgered<String, PreparedBuild>,
    prepared_queries: Ledgered<String, PreparedQueryCursor>,
    expired_prepared_queries: Ledgered<String, (String, u64)>,
    prepared_facts: Ledgered<PreparedDiskKey, CachedPreparedFacts>,
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
    catalogs: Ledgered<String, Arc<CatalogSlot>>,
    registered_builds: Ledgered<String, RegisteredBuild>,
    escaped_chunks: Table<usize, (ChunkAllocationOwner, MemoryCharge)>,
    recent_codex_raw_bytes: usize,
    locations_expiry: ExpiryIndex<String>,
    carried_expiry: ExpiryIndex<(SourceIdentity, String)>,
    recent_codex_expiry: ExpiryIndex<SourceIdentity>,
    prepared_loads_expiry: ExpiryIndex<SourceIdentity>,
    prepared_facts_lru: BTreeSet<(u64, PreparedDiskKey)>,
    counters: [u64; 20],
    transient_bytes: usize,
    prepared_disk_index_bytes: usize,
    fact_read_in_flight: bool,
    owned: crate::snapshot_owned::OwnedProjections,
    ledger: RetainedLedger,
    #[cfg(test)]
    audits: Arc<AtomicUsize>,
    #[cfg(test)]
    retained_owners: Vec<Weak<crate::snapshot_prepared::PreparedFacts>>,
}

const TOUCH_TTL_MS: u64 = 30 * 60_000;
const EXPIRED_QUERY_TOMBSTONES: usize = 1024;
const FACT_READ_WAIT_SLICE: Duration = Duration::from_millis(25);

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
            gauge_reports: Work::default(),
            latest: Table::new(work.clone()),
            source_spines: Table::new(work.clone()),
            source_view_nodes: 0,
            source_view_checks: Work::default(),
            classified: Ledgered::new(work.clone()),
            cache_order: BTreeSet::new(),
            cache_clock: 0,
            carried_owners: Ledgered::new(work.clone()),
            catalog_owners: Ledgered::new(work.clone()),
            recent_codex: Table::new(work.clone()),
            loads: Table::new(work.clone()),
            prepared_loads: Table::new(work.clone()),
            leases: Ledgered::new(work.clone()),
            waiters: Ledgered::new(work.clone()),
            projections: Ledgered::new(work.clone()),
            generations: Ledgered::new(work.clone()),
            classifier_stages: Table::new(work.clone()),
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
            catalogs: Ledgered::new(work.clone()),
            registered_builds: Ledgered::new(work.clone()),
            escaped_chunks: Table::new(work.clone()),
            recent_codex_raw_bytes: 0,
            locations_expiry: ExpiryIndex::new(work.clone()),
            carried_expiry: ExpiryIndex::new(work.clone()),
            recent_codex_expiry: ExpiryIndex::new(work.clone()),
            prepared_loads_expiry: ExpiryIndex::new(work.clone()),
            prepared_facts_lru: BTreeSet::new(),
            counters: [0; 20],
            transient_bytes: 0,
            prepared_disk_index_bytes: 0,
            fact_read_in_flight: false,
            owned: crate::snapshot_owned::OwnedProjections::default(),
            ledger: RetainedLedger::new(work),
            #[cfg(test)]
            audits: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            retained_owners: Vec::new(),
        }
    }

    #[cfg(test)]
    fn reservables(&self) -> [&dyn Reserved; 37] {
        [
            &self.ledger.shared,
            &self.registries,
            &self.generations,
            &self.carried_classifications,
            &self.leases,
            &self.waiters,
            &self.deliveries,
            &self.projections,
            &self.graphs,
            &self.prepared_graphs,
            &self.prepared_builds,
            &self.prepared_queries,
            &self.expired_prepared_queries,
            &self.prepared_facts,
            &self.labels,
            &self.discoveries,
            &self.checkpoints,
            &self.resolutions,
            &self.loads,
            &self.classifier_stages,
            &self.latest,
            &self.source_spines,
            &self.classified,
            &self.carried_owners,
            &self.catalog_owners,
            &self.escaped_chunks,
            &self.prepared_loads,
            &self.recent_codex,
            &self.warm_memberships,
            &self.locations,
            &self.locates,
            &self.catalogs,
            &self.registered_builds,
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
        mut record: GenerationRecord,
    ) {
        let key = snapshot_key(snapshot);
        let mut carried_raw_window = None;
        if let Some(mut previous) = self.generations.get_mut(&key) {
            record.cached = previous.cached.take();
            carried_raw_window = previous.raw_window.take();
            if let Some(window) = &carried_raw_window {
                record.raw_window = Some(Arc::clone(window));
            }
            record.raw_parent = previous.raw_parent.take();
            record.raw_cache = previous.raw_cache.take();
            record.interactions = previous.interactions.take();
        }
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
        for anchor in carried_raw_window.iter().flat_map(raw_window_anchors) {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn unregister_generation(&mut self, snapshot: usize) {
        if let Some(record) = self.generations.remove(&snapshot) {
            assert!(
                record.cached.is_none(),
                "generation cache owner is still live"
            );
            for anchor in record.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
    }

    fn escaping<'a>(&'a self, chunks: &'a [Arc<EntryChunk>]) -> impl Iterator<Item = Anchor> + 'a {
        chunks
            .iter()
            .flat_map(|chunk| {
                [
                    Anchor::entries(chunk_key(chunk), chunk.rows_charge()),
                    Anchor::entries(
                        Arc::as_ptr(chunk) as usize,
                        MemoryCharge {
                            owned_capacity_bytes: chunk.metadata_bytes(),
                            opaque_dom_accounted_bytes: 0,
                        },
                    ),
                ]
            })
            .filter(|anchor| !self.escaped_chunks.contains_key(&anchor.id))
    }

    fn escape_growth(&self, chunks: &[Arc<EntryChunk>]) -> usize {
        self.escaped_chunks.growth(self.escaping(chunks).count())
            + self.ledger.shared.admission(self.escaping(chunks))
    }

    fn escape_chunks(&mut self, chunks: &[Arc<EntryChunk>]) {
        let anchors: Vec<_> = self.escaping(chunks).collect();
        self.escaped_chunks.reserve(anchors.len());
        self.ledger.shared.reserve(anchors);
        for chunk in chunks {
            let rows = chunk_key(chunk);
            if !self.escaped_chunks.contains_key(&rows) {
                chunk
                    .entries
                    .ledger
                    .arm(&self.ledger.queue, LedgerEvent::Chunk(rows));
                let charge = chunk.rows_charge();
                self.escaped_chunks.insert(
                    rows,
                    (
                        ChunkAllocationOwner::Rows(Arc::downgrade(&chunk.entries)),
                        charge,
                    ),
                );
                self.ledger.shared.acquire(Anchor::entries(rows, charge));
            }
            let metadata = Arc::as_ptr(chunk) as usize;
            if !self.escaped_chunks.contains_key(&metadata) {
                chunk
                    .ledger
                    .arm(&self.ledger.queue, LedgerEvent::Chunk(metadata));
                let charge = MemoryCharge {
                    owned_capacity_bytes: chunk.metadata_bytes(),
                    opaque_dom_accounted_bytes: 0,
                };
                self.escaped_chunks.insert(
                    metadata,
                    (
                        ChunkAllocationOwner::Metadata(Arc::downgrade(chunk)),
                        charge,
                    ),
                );
                self.ledger
                    .shared
                    .acquire(Anchor::entries(metadata, charge));
            }
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

    fn carried_growth(
        &self,
        lineage: &(SourceIdentity, String),
        carried: &Arc<CarriedClassification>,
    ) -> usize {
        self.carried_owner_growth(lineage, carried)
            + self.carried_classifications.growth_for(lineage)
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
        if let Some(previous) = self.carried_classifications.get(&lineage).cloned() {
            self.detach_carried_owner(&lineage, &previous);
            for anchor in previous.anchors() {
                self.ledger.shared.release(anchor.id);
            }
        }
        for anchor in carried.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        self.carried_expiry
            .push(carried_deadline(&carried), lineage.clone());
        self.attach_carried_owner(&lineage, &carried);
        self.carried_classifications.insert(lineage, carried);
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
            self.detach_carried_owner(lineage, &carried);
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

    fn holds_latest(&self, identity: &SourceIdentity, snapshot: &TranscriptSnapshot) -> bool {
        self.latest
            .get(identity)
            .is_some_and(|key| *key == snapshot as *const TranscriptSnapshot as usize)
    }

    fn insert_latest(
        &mut self,
        identity: SourceIdentity,
        snapshot: Arc<TranscriptSnapshot>,
    ) -> Option<Arc<TranscriptSnapshot>> {
        let key = snapshot_key(&snapshot);
        let previous = self.latest.get(&identity).copied();
        let displaced = previous.map(|old| {
            Arc::clone(
                &self
                    .generations
                    .get(&old)
                    .expect("latest generation")
                    .cached
                    .as_ref()
                    .expect("cached owner")
                    .snapshot,
            )
        });
        if let Some(old) = previous.filter(|old| *old != key) {
            self.generations
                .get_mut(&old)
                .expect("latest generation")
                .cached
                .as_mut()
                .expect("cached owner")
                .latest = None;
            self.forget_source_spine(&identity, displaced.as_ref().expect("displaced source"));
            self.release_unused_cached_generation(old);
        }
        let physical = identity.file();
        self.cache_generation(Arc::clone(&snapshot));
        self.latest.reserve_for(&identity);
        self.latest.insert(identity, key);
        self.generations
            .get_mut(&key)
            .expect("latest generation")
            .cached
            .as_mut()
            .expect("cached owner")
            .latest = Some(identity);
        self.retire_incompatible_source_views(&snapshot);
        let indexed = self
            .generations
            .get(&key)
            .expect("source generation")
            .cached
            .as_ref()
            .expect("cached owner")
            .spine
            .is_some();
        if !indexed {
            self.source_spines.reserve_for(&physical);
            if !self.source_spines.contains_key(&physical) {
                self.source_spines.insert(physical, BTreeSet::new());
            }
            assert!(self
                .source_spines
                .get_mut(&physical)
                .expect("source views")
                .insert(SourceView::of(&snapshot)));
            self.source_view_nodes += 1;
            self.generations
                .get_mut(&key)
                .expect("source generation")
                .cached
                .as_mut()
                .expect("cached owner")
                .spine = Some(physical);
        }
        if self.recent_codex.contains_key(&identity) {
            self.recent_codex_raw_bytes -= displaced.as_ref().map_or(0, codex_raw_len);
            self.recent_codex_raw_bytes += codex_raw_len(&snapshot);
        }
        displaced
    }

    fn remove_latest(&mut self, identity: &SourceIdentity) -> Option<Arc<TranscriptSnapshot>> {
        let key = self.latest.remove(identity)?;
        let removed = Arc::clone(
            &self
                .generations
                .get(&key)
                .expect("latest generation")
                .cached
                .as_ref()
                .expect("cached owner")
                .snapshot,
        );
        let mut generation = self.generations.get_mut(&key).expect("latest generation");
        let cached = generation.cached.as_mut().expect("cached owner");
        cached.latest = None;
        let indexed = cached.spine.take().is_some();
        drop(generation);
        if indexed {
            self.remove_source_view(&removed);
        }
        self.release_unused_cached_generation(key);
        if self.recent_codex.contains_key(identity) {
            self.recent_codex_raw_bytes -= codex_raw_len(&removed);
        }
        Some(removed)
    }

    #[cfg(test)]
    fn retain_latest(
        &mut self,
        mut keep: impl FnMut(&SourceIdentity, &Arc<TranscriptSnapshot>) -> bool,
    ) {
        let removed: Vec<_> = self
            .latest
            .iter()
            .filter_map(|(identity, key)| {
                self.ledger.shared.work().tick(1);
                let snapshot = &self
                    .generations
                    .get(key)
                    .expect("latest generation")
                    .cached
                    .as_ref()
                    .expect("cached owner")
                    .snapshot;
                (!keep(identity, snapshot)).then_some(*identity)
            })
            .collect();
        for identity in removed {
            self.remove_latest(&identity);
        }
    }

    fn latest_raw_bytes(&self, identity: &SourceIdentity) -> usize {
        self.latest.get(identity).map_or(0, |key| {
            codex_raw_len(
                &self
                    .generations
                    .get(key)
                    .expect("latest generation")
                    .cached
                    .as_ref()
                    .expect("cached owner")
                    .snapshot,
            )
        })
    }

    fn insert_recent_codex(&mut self, identity: SourceIdentity, now: u64) {
        if !self.recent_codex.contains_key(&identity) {
            self.recent_codex.reserve(1);
            self.recent_codex_expiry.reserve(1);
        }
        if self.recent_codex.insert(identity, now).is_some() {
            return;
        }
        self.recent_codex_raw_bytes += self.latest_raw_bytes(&identity);
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
            self.recent_codex_raw_bytes -= self.latest_raw_bytes(identity);
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

    #[cfg(test)]
    fn retain_carried(
        &mut self,
        mut keep: impl FnMut(&(SourceIdentity, String), &Arc<CarriedClassification>) -> bool,
    ) {
        let removed: Vec<_> = self
            .carried_classifications
            .iter()
            .filter_map(|(lineage, carried)| (!keep(lineage, carried)).then(|| lineage.clone()))
            .collect();
        for lineage in removed {
            self.remove_carried(&lineage);
        }
    }

    fn insert_label(&mut self, token: String, slot: LabelSlot) {
        self.labels.reserve_for(&token);
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

    fn extract_label(
        &mut self,
        token: &str,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Option<LabelSlot> {
        let slot = self.remove_label(token)?;
        let held = slot.accounted + self.ledger.shared.unowned_bytes(slot.anchors());
        self.transient_bytes += held;
        reservation.bytes += held;
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
        self.classifier_stages.reserve_for(&key);
        self.ledger.shared.reserve(slot.anchors());
        for anchor in slot.anchors() {
            self.ledger.shared.acquire(anchor);
        }
        slot.attached.store(true, Ordering::Release);
        self.ledger.classifier +=
            slot.record_bytes() + key.capacity() + slot.accounted.load(Ordering::Acquire);
        if let Some(displaced) = self.classifier_stages.insert(key, slot) {
            self.detach_classifier_stage(0, &displaced);
        }
    }

    fn detach_classifier_stage(&mut self, key_bytes: usize, slot: &ClassifierSlot) {
        slot.attached.store(false, Ordering::Release);
        self.ledger.classifier = self
            .ledger
            .classifier
            .checked_sub(slot.record_bytes() + key_bytes + slot.accounted.load(Ordering::Acquire))
            .expect("balanced retained ledger");
        for anchor in slot.anchors() {
            self.ledger.shared.release(anchor.id);
        }
    }

    fn remove_classifier_stage(&mut self, key: &str) -> Option<Arc<ClassifierSlot>> {
        let (key, slot) = self.classifier_stages.remove_entry(key)?;
        self.detach_classifier_stage(key.capacity(), &slot);
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
        self.loads.reserve_for(&identity);
        slot.attached.store(true, Ordering::Release);
        self.ledger.pending += slot.record_bytes() + slot.accounted.load(Ordering::Acquire);
        if let Some(displaced) = self.loads.insert(identity, slot) {
            self.detach_load(&displaced);
        }
    }

    fn detach_load(&mut self, slot: &LoadSlot) {
        slot.attached.store(false, Ordering::Release);
        self.ledger.pending = self
            .ledger
            .pending
            .checked_sub(slot.record_bytes() + slot.accounted.load(Ordering::Acquire))
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

    fn prepared_facts_growth(&self, key: &PreparedDiskKey) -> usize {
        self.prepared_facts.growth_for(key)
            + usize::from(!self.prepared_facts.contains_key(key))
                * size_of::<(u64, PreparedDiskKey)>()
    }

    fn insert_prepared_facts(&mut self, key: PreparedDiskKey, cached: CachedPreparedFacts) {
        self.prepared_facts.reserve_for(&key);
        self.ledger.shared.reserve([facts_anchor(&cached.facts)]);
        self.ledger.shared.acquire(facts_anchor(&cached.facts));
        let last_used = cached.last_used;
        if let Some(displaced) = self.prepared_facts.insert(key, cached) {
            self.ledger.shared.release(facts_key(&displaced.facts));
            self.prepared_facts_lru.remove(&(displaced.last_used, key));
        }
        self.prepared_facts_lru.insert((last_used, key));
    }

    fn remove_prepared_facts(&mut self, key: &PreparedDiskKey) -> Option<CachedPreparedFacts> {
        let cached = self.prepared_facts.remove(key)?;
        self.ledger.shared.release(facts_key(&cached.facts));
        self.prepared_facts_lru.remove(&(cached.last_used, *key));
        Some(cached)
    }

    fn identity_facts(
        &self,
        identity: SourceIdentity,
    ) -> impl Iterator<Item = (&PreparedDiskKey, &CachedPreparedFacts)> {
        let work = self.ledger.shared.work();
        self.prepared_facts
            .iter()
            .inspect(move |_| work.tick(1))
            .filter(move |(key, _)| key.identity() == identity)
    }

    fn retire_identity_facts(
        &mut self,
        identity: SourceIdentity,
        keep: impl Fn(&CachedPreparedFacts) -> bool,
    ) -> usize {
        let Self {
            prepared_facts,
            prepared_facts_lru,
            ledger,
            ..
        } = self;
        let mut retired = 0usize;
        prepared_facts.retain(|key, cached| {
            if key.identity() != identity || keep(cached) {
                return true;
            }
            ledger.shared.release(facts_key(&cached.facts));
            prepared_facts_lru.remove(&(cached.last_used, *key));
            retired += 1;
            false
        });
        retired
    }

    fn remove_identity_facts(&mut self, identity: SourceIdentity) -> usize {
        self.retire_identity_facts(identity, |_| false)
    }

    #[cfg(test)]
    fn has_prepared_facts(&self, identity: &SourceIdentity) -> bool {
        self.identity_facts(*identity).next().is_some()
    }

    #[cfg(test)]
    fn cached_prepared_facts(&self, identity: &SourceIdentity) -> &CachedPreparedFacts {
        let mut bindings = self.identity_facts(*identity).map(|(_, cached)| cached);
        let cached = bindings.next().expect("cached facts");
        assert!(
            bindings.next().is_none(),
            "several bindings are cached for one identity"
        );
        cached
    }

    fn evictable_prepared_facts(&self) -> Option<PreparedDiskKey> {
        let work = self.ledger.shared.work();
        self.prepared_facts_lru
            .iter()
            .map(|(_, key)| *key)
            .inspect(|_| work.tick(1))
            .find(|key| self.prepared_facts[key].evictable())
    }

    fn prepared_facts_fit_sparing(
        &self,
        key: PreparedDiskKey,
        accounted: usize,
        budget: usize,
        spared: impl Fn(&CachedPreparedFacts) -> bool,
    ) -> bool {
        if accounted > budget {
            return false;
        }
        let work = self.ledger.shared.work();
        let mut used = self.ledger.shared.facts()
            - self
                .prepared_facts
                .get(&key)
                .filter(|cached| cached.evictable())
                .map_or(0, |cached| cached.facts.accounted_bytes());
        for (_, victim) in &self.prepared_facts_lru {
            if accounted <= budget.saturating_sub(used) {
                return true;
            }
            if *victim == key {
                continue;
            }
            work.tick(1);
            let cached = &self.prepared_facts[victim];
            if !cached.evictable() {
                continue;
            }
            if spared(cached) {
                return false;
            }
            used -= cached.facts.accounted_bytes();
        }
        accounted <= budget.saturating_sub(used)
    }

    fn touch_prepared_facts(
        &mut self,
        key: &PreparedDiskKey,
        stamp: SourceStamp,
        registry_generation: &str,
        admission: &str,
        authority: &Value,
        classifier: &Value,
        need: FactsNeed,
    ) -> Option<Arc<crate::snapshot_prepared::PreparedFacts>> {
        let mut cached = self.prepared_facts.get_mut(key)?;
        if !cached.serves(
            stamp,
            registry_generation,
            admission,
            authority,
            classifier,
            need,
        ) {
            return None;
        }
        let previous = cached.last_used;
        cached.last_used = now_ms();
        self.prepared_facts_lru.remove(&(previous, *key));
        self.prepared_facts_lru.insert((cached.last_used, *key));
        Some(Arc::clone(&cached.facts))
    }

    fn retained_facts<'a>(
        &mut self,
        store: &'a NativeStore,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    ) -> RetainedFacts<'a> {
        #[cfg(test)]
        self.retained_owners.push(Arc::downgrade(&facts));
        RetainedFacts {
            store,
            facts: Some(facts),
        }
    }

    fn insert_prepared_graph(&mut self, graph_id: String, graph: Arc<Mutex<PreparedGraph>>) {
        self.prepared_graphs.reserve_for(&graph_id);
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
        if let Some(displaced) = graph.root_slices.insert(key, facts) {
            self.ledger.shared.release(facts_key(&displaced));
        }
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

    fn consume_delivery_pledge(&mut self, cursor: &str) -> usize {
        self.labels.consume_pledge(cursor)
            + self.graphs.consume_pledge(cursor)
            + self.prepared_builds.consume_pledge(cursor)
            + self.registered_builds.consume_pledge(cursor)
            + self.prepared_queries.consume_pledge(cursor)
            + self.projections.consume_pledge(cursor)
            + self.discoveries.consume_pledge(cursor)
            + self.resolutions.consume_pledge(cursor)
            + self.locates.consume_pledge(cursor)
            + self.waiters.consume_pledge(cursor)
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
        self.prepared_builds.reserve_for(&token);
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
                detached.push((key.capacity(), Arc::clone(slot)));
            }
            kept
        });
        for (key_bytes, slot) in detached {
            self.detach_classifier_stage(key_bytes, &slot);
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
    fn totals(&self, state: &StoreState) -> (usize, usize, usize) {
        let metadata = self.metadata + NativeStore::fixed_metadata_bytes(state);
        let owned = state.owned.accounted_bytes();
        (
            self.projections + self.discovery + self.graph + metadata + owned,
            self.pending + self.classifier + self.label + state.transient_bytes,
            self.entries
                + self.indexes
                + self.pending
                + self.classifier
                + self.label
                + self.projections
                + self.discovery
                + self.graph
                + metadata
                + state.transient_bytes
                + owned,
        )
    }

    fn report(&self, state: &StoreState) -> Value {
        state.gauge_reports.tick(1);
        let (projections, pending, total) = self.totals(state);
        json!({"retained_entry_capacity_bytes": self.entries, "retained_index_capacity_bytes": self.indexes,
            "retained_projection_bytes": projections, "pending_input_capacity_bytes": pending,
            "retained_total_accounted_bytes": total,
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
    transport_bytes: AtomicU64,
    state: Mutex<StoreState>,
    fact_read_released: Condvar,
    #[cfg(test)]
    catalog_wait_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    classifiers: Mutex<HashMap<String, Arc<ClassifierCallback>>>,
    policies: Mutex<HashMap<String, Arc<PolicyCallback>>>,
    #[cfg(test)]
    read_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    decode_hook: Mutex<Option<Arc<dyn Fn(&'static str) + Send + Sync>>>,
    #[cfg(test)]
    pin_hook: Mutex<Option<Arc<dyn Fn(&Value) -> Result<(), SnapshotError> + Send + Sync>>>,
    #[cfg(test)]
    locate_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    location_metadata_hook: Mutex<Option<(&'static str, Arc<dyn Fn() + Send + Sync>)>>,
    #[cfg(test)]
    directory_force_unknown: AtomicBool,
    #[cfg(test)]
    directory_descriptor_metadata: AtomicUsize,
    #[cfg(test)]
    location_entry_canonicalizations: AtomicUsize,
    #[cfg(test)]
    location_canonicalizations: AtomicUsize,
    #[cfg(test)]
    catalog_id_comparisons: AtomicUsize,
    #[cfg(test)]
    release_hook: Mutex<Option<Arc<dyn Fn(usize) + Send + Sync>>>,
    #[cfg(test)]
    resolve_hook: Mutex<Option<Arc<dyn Fn(&Path) + Send + Sync>>>,
    #[cfg(test)]
    membership_metadata_checks: AtomicUsize,
    #[cfg(test)]
    pub(crate) source_resolutions: AtomicUsize,
    #[cfg(test)]
    pub(crate) retained_work: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) reclaims: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) warm_copies: AtomicUsize,
    #[cfg(test)]
    pub(crate) fact_builds: AtomicUsize,
    #[cfg(test)]
    pub(crate) description_builds: AtomicUsize,
    #[cfg(test)]
    pub(crate) fact_lookups: AtomicUsize,
    #[cfg(test)]
    pub(crate) fact_read_waits: AtomicUsize,
    #[cfg(test)]
    pub(crate) built_facts_hook:
        Mutex<Option<Arc<dyn Fn(&Arc<crate::snapshot_prepared::PreparedFacts>) + Send + Sync>>>,
    #[cfg(test)]
    pub(crate) returned_facts_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pub(crate) build_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) build_sources: AtomicUsize,
    #[cfg(test)]
    pub(crate) graph_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) graph_sources: AtomicUsize,
    #[cfg(test)]
    pub(crate) locate_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) locate_items: AtomicUsize,
    #[cfg(test)]
    pub(crate) registered_sources: AtomicUsize,
    #[cfg(test)]
    pub(crate) discovery_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) resolution_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) prepared_query_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) warm_records: AtomicUsize,
    #[cfg(test)]
    pub(crate) directory_opens: AtomicUsize,
    #[cfg(test)]
    pub(crate) audits: Arc<AtomicUsize>,
}

pub(crate) struct ProjectionReservation<'a> {
    store: &'a NativeStore,
    bytes: usize,
}

impl Drop for ProjectionReservation<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            let hook = self
                .store
                .release_hook
                .lock()
                .expect("release hook")
                .clone();
            if let Some(hook) = hook {
                hook(self.bytes);
            }
        }
        self.store.lock_state().transient_bytes -= self.bytes;
    }
}

pub(crate) struct FactReadClaim<'a> {
    store: &'a NativeStore,
}

impl Drop for FactReadClaim<'_> {
    fn drop(&mut self) {
        self.store.lock_state().fact_read_in_flight = false;
        self.store.fact_read_released.notify_all();
    }
}

struct RetainedFacts<'a> {
    store: &'a NativeStore,
    facts: Option<Arc<crate::snapshot_prepared::PreparedFacts>>,
}

impl RetainedFacts<'_> {
    fn facts(&self) -> &Arc<crate::snapshot_prepared::PreparedFacts> {
        self.facts
            .as_ref()
            .expect("retained facts are released once")
    }
}

impl Drop for RetainedFacts<'_> {
    fn drop(&mut self) {
        let mut state = self.store.lock_state();
        let facts = self.facts.take().expect("retained facts are released once");
        state.ledger.shared.release(facts_key(&facts));
        #[cfg(test)]
        {
            let registered = state
                .retained_owners
                .iter()
                .position(|owner| owner.as_ptr() == Arc::as_ptr(&facts))
                .expect("retained facts are registered");
            state.retained_owners.swap_remove(registered);
        }
        drop(facts);
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

fn prepared_cache_read_limit() -> SnapshotError {
    SnapshotError::new(Status::Incomplete, "prepared_cache_read_limit")
}

fn prepared_cache_entry_limit() -> SnapshotError {
    SnapshotError::new(
        Status::SourceLimit,
        "prepared facts entry exceeds the warming read budget",
    )
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

fn active_since_ns(request: &Value) -> Result<Option<i128>, SnapshotError> {
    request
        .get("active_since_unix_ms")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|since| *since > 0)
                .map(|since| i128::from(since) * 1_000_000)
                .ok_or_else(|| invalid("active_since_unix_ms must be a positive integer"))
        })
        .transpose()
}

fn active(source: &PreparedSourceRef, since: Option<i128>) -> bool {
    since.is_none_or(|since| source.stamp.mtime_ns >= since)
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

fn usage_value(usage: &[u64; 20]) -> Value {
    let mut result = json!({});
    for (key, count) in COUNTERS.iter().zip(usage.iter()).take(20) {
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
            scan_read: get("max_scan_read_bytes", 1 << 40),
            scan_events: get("max_scan_events", 1 << 32),
            scan_discovery: get("max_scan_discovery_entries", 1 << 20),
            scan_sources: get("max_scan_sources", 1 << 20),
            scan_validate: get("max_scan_validate_bytes", 1024 * 1024 * 1024),
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
            || config.scan_read == 0
            || config.scan_events == 0
            || config.scan_discovery == 0
            || config.scan_sources == 0
            || config.scan_validate == 0
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
        state.deliveries.reserve(config.leases.saturating_mul(4));
        state
            .expired_prepared_queries
            .reserve(EXPIRED_QUERY_TOMBSTONES);
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
        let footprint = Self::retained_total(&mut state);
        if footprint > config.retained {
            return Err(invalid("retained cap below startup footprint"));
        }
        #[cfg(test)]
        let audits = Arc::clone(&state.audits);
        let prepared_disk = crate::snapshot_prepared_disk::PreparedDiskCache::new(
            &owner_epoch,
            get("max_prepared_disk_bytes", 2 * 1024 * 1024 * 1024),
            &crate::snapshot_prepared_disk::FactOrigin {
                producer: value.get("prepared_fact_producer").and_then(Value::as_str),
                engine: crate::BUILD_IDENTITY,
                max_source_bytes: config.source,
                max_entry_bytes: config.entry,
            },
            config
                .retained
                .saturating_sub(config.hook_bytes)
                .saturating_sub(footprint),
            work.clone(),
        )?;
        state.prepared_disk_index_bytes = prepared_disk.index_bytes();
        Ok(Self {
            config,
            prepared_disk,
            owner_epoch,
            default_registry,
            seed,
            sequence: AtomicUsize::new(1),
            transport_bytes: AtomicU64::new(0),
            state: Mutex::new(state),
            fact_read_released: Condvar::new(),
            #[cfg(test)]
            catalog_wait_hook: Mutex::new(None),
            classifiers: Mutex::new(HashMap::new()),
            policies: Mutex::new(HashMap::new()),
            #[cfg(test)]
            read_hook: Mutex::new(None),
            #[cfg(test)]
            decode_hook: Mutex::new(None),
            #[cfg(test)]
            pin_hook: Mutex::new(None),
            #[cfg(test)]
            locate_hook: Mutex::new(None),
            #[cfg(test)]
            location_metadata_hook: Mutex::new(None),
            #[cfg(test)]
            directory_force_unknown: AtomicBool::new(false),
            #[cfg(test)]
            directory_descriptor_metadata: AtomicUsize::new(0),
            #[cfg(test)]
            location_entry_canonicalizations: AtomicUsize::new(0),
            #[cfg(test)]
            location_canonicalizations: AtomicUsize::new(0),
            #[cfg(test)]
            catalog_id_comparisons: AtomicUsize::new(0),
            #[cfg(test)]
            release_hook: Mutex::new(None),
            #[cfg(test)]
            resolve_hook: Mutex::new(None),
            #[cfg(test)]
            membership_metadata_checks: AtomicUsize::new(0),
            #[cfg(test)]
            source_resolutions: AtomicUsize::new(0),
            #[cfg(test)]
            retained_work: work.counter(),
            #[cfg(test)]
            reclaims: work.reclaims(),
            #[cfg(test)]
            warm_copies: AtomicUsize::new(0),
            #[cfg(test)]
            fact_builds: AtomicUsize::new(0),
            #[cfg(test)]
            description_builds: AtomicUsize::new(0),
            #[cfg(test)]
            fact_lookups: AtomicUsize::new(0),
            #[cfg(test)]
            fact_read_waits: AtomicUsize::new(0),
            #[cfg(test)]
            built_facts_hook: Mutex::new(None),
            #[cfg(test)]
            returned_facts_hook: Mutex::new(None),
            #[cfg(test)]
            build_records: AtomicUsize::new(0),
            #[cfg(test)]
            build_sources: AtomicUsize::new(0),
            #[cfg(test)]
            graph_records: AtomicUsize::new(0),
            #[cfg(test)]
            graph_sources: AtomicUsize::new(0),
            #[cfg(test)]
            locate_records: AtomicUsize::new(0),
            #[cfg(test)]
            locate_items: AtomicUsize::new(0),
            #[cfg(test)]
            registered_sources: AtomicUsize::new(0),
            #[cfg(test)]
            discovery_records: AtomicUsize::new(0),
            #[cfg(test)]
            resolution_records: AtomicUsize::new(0),
            #[cfg(test)]
            prepared_query_records: AtomicUsize::new(0),
            #[cfg(test)]
            warm_records: AtomicUsize::new(0),
            #[cfg(test)]
            directory_opens: AtomicUsize::new(0),
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
            max_read_bytes: self.config.scan_read,
            max_source_read_bytes: self.config.scan_read,
            max_events: self.config.scan_events,
            max_items: self.config.scan_events,
            max_output_bytes: self.config.output,
            max_discovery_entries: self.config.scan_discovery,
            max_sources: self.config.scan_sources,
            deadline_unix_ms: now_ms().saturating_add(self.scan_timeout_ms()),
        }
    }

    pub fn scan_timeout_ms(&self) -> u64 {
        self.config.preparation
    }

    pub fn scan_validate_bytes(&self) -> usize {
        self.config.scan_validate
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
            self.pin_record(handle, context)?;
        }
        let state = self.lock_state();
        let mut expires = (now_ms() + self.config.ttl).min(deadline);
        for handle in handles {
            expires = expires.min(self.lease(&state, handle, context)?.expires);
        }
        Ok(expires)
    }

    pub(crate) fn claim_fact_read(
        &self,
        cancel: &Cancellation,
        deadline_unix_ms: u64,
    ) -> Result<FactReadClaim<'_>, SnapshotError> {
        let mut state = self.lock_state();
        #[cfg(test)]
        {
            if state.fact_read_in_flight {
                self.fact_read_waits.fetch_add(1, Ordering::Relaxed);
            }
        }
        while state.fact_read_in_flight {
            state = self
                .fact_read_released
                .wait_timeout(state, FACT_READ_WAIT_SLICE)
                .expect("snapshot state")
                .0;
            cancel.check(deadline_unix_ms)?;
        }
        state.fact_read_in_flight = true;
        Ok(FactReadClaim { store: self })
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
        state.ledger.shared.work().reserved(bytes);
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
        self.extend_projection_reservation_in(&mut self.lock_state(), reservation, context, bytes)
    }

    fn extend_projection_reservation_in(
        &self,
        state: &mut StoreState,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        bytes: usize,
    ) -> Result<(), SnapshotError> {
        self.admit_memory(state, context, bytes)?;
        state.transient_bytes += bytes;
        reservation.bytes += bytes;
        state.ledger.shared.work().reserved(bytes);
        Ok(())
    }

    fn retain_facts(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    ) -> Result<RetainedFacts<'_>, SnapshotError> {
        if !std::ptr::eq(self, reservation.store) {
            return Err(invalid("projection reservation belongs to another owner"));
        }
        self.after_facts_returned();
        let anchor = facts_anchor(&facts);
        let mut state = self.lock_state();
        let covered = state
            .ledger
            .shared
            .unowned_bytes([anchor])
            .min(reservation.bytes);
        let fresh = state.ledger.shared.admission([anchor]) - covered;
        self.admit_memory(&mut state, context, fresh)?;
        state.ledger.shared.reserve([anchor]);
        state.ledger.shared.acquire(anchor);
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        Ok(state.retained_facts(self, facts))
    }

    fn retain_cached_facts(
        &self,
        state: &mut StoreState,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
    ) -> RetainedFacts<'_> {
        state.ledger.shared.acquire(facts_anchor(&facts));
        state.retained_facts(self, facts)
    }

    fn touch_retained_facts(
        &self,
        key: &PreparedDiskKey,
        stamp: SourceStamp,
        registry_generation: &str,
        admission: &str,
        authority: &Value,
        classifier: &Value,
        need: FactsNeed,
    ) -> Option<RetainedFacts<'_>> {
        let mut state = self.lock_state();
        let facts = state.touch_prepared_facts(
            key,
            stamp,
            registry_generation,
            admission,
            authority,
            classifier,
            need,
        )?;
        Some(self.retain_cached_facts(&mut state, facts))
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
        let used = Self::retained_total(&mut state);
        let available = cap.saturating_sub(used);
        let preferred_growth = preferred.saturating_sub(reservation.bytes);
        let growth = preferred_growth.min(available);
        state.transient_bytes += growth;
        reservation.bytes += growth;
        state.ledger.shared.work().reserved(growth);
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
        let mut usage = [0u64; 20];
        usage[6] = activity_lifts as u64;
        reply.insert("work",json!({"events":work.events,"input_bytes":work.input_bytes,"items":work.items,"output_bytes":work.output_bytes}));
        reply.insert("usage", usage_value(&usage));
        self.track_delivery(&reply, context, true)?;
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
            + handle_charge.opaque_dom_accounted_bytes
            + slot.admission.capacity();
        let pledge = Delivery::cursor_pledge(str_field(context, "claimant")?, &token);
        self.extend_projection_reservation(reservation, context, pledge)?;
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
        let retained = charged_bytes(&token, &slot)
            + state.labels.growth_for(&token)
            + state.ledger.shared.admission(slot.anchors())
            + pledge;
        if retained > reservation.bytes {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "classifier retained bytes exceed reservation",
            ));
        }
        state.transient_bytes -= retained;
        reservation.bytes -= retained;
        state.insert_label(token.clone(), slot);
        state.labels.pledge(&token, pledge);
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
        let pledge = Lease::pledge(&token, context, &classifier)?;
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
                    + state.carried_growth(lineage, candidate)
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
        let scope = self.renew_scope_for_work(handle, context, bounds.deadline_unix_ms)?;
        let source = scope.snapshot;
        bounds.deadline_unix_ms = bounds.deadline_unix_ms.min(scope.expires);
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
        let max_stage_bytes = entry_bytes(&source)
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
            state
                .extract_label(cursor, &mut reservation)
                .ok_or_else(|| {
                    SnapshotError::new(Status::StaleCursor, "classifier cursor already consumed")
                })?
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
        self.transport_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn reported_counters(&self, state: &StoreState) -> [u64; 20] {
        let mut counters = state.counters;
        counters[14] += self.transport_bytes.load(Ordering::Relaxed);
        counters
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
        usage: &mut [u64; 20],
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
            if let Some(derived) = state.classified_source(&key) {
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
                let stored = key.clone();
                let additional = CLASSIFIER_SLOT_BYTES
                    + snapshot.chunks.len() * size_of::<Arc<EntryChunk>>()
                    + stored.capacity()
                    + accounted
                    + state.classifier_stages.growth_for(&stored)
                    + state
                        .ledger
                        .shared
                        .growth(seed.iter().flat_map(|seed| seed.anchors()));
                self.admit_memory(&mut state, context, additional)?;
                let slot = Arc::new(ClassifierSlot {
                    work: Mutex::new(stage),
                    seed,
                    chunks: snapshot.chunks.clone(),
                    accounted: AtomicUsize::new(accounted),
                    attached: AtomicBool::new(false),
                    deadline: now_ms() + self.config.preparation,
                    complete: AtomicBool::new(false),
                });
                state.insert_classifier_stage(stored, Arc::clone(&slot));
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
        let reserve = if stop > start {
            let (calls, results) = (start..stop).map(|position| snapshot.entry(position)).fold(
                (0usize, 0usize),
                |(calls, results), entry| {
                    (
                        calls + entry.tool_uses().count(),
                        results + entry.tool_results().count(),
                    )
                },
            );
            bytes
                .saturating_mul(2)
                .saturating_add((stop - start) * (size_of::<&Entry>() + size_of::<bool>()))
                .saturating_add(stage.activity.append_container_reservation_bytes(
                    stop - start,
                    calls,
                    results,
                ))
        } else {
            0
        };
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
        let accounted = stage.accounted_bytes();
        {
            let mut state = self.lock_state();
            if let Err(error) = self.admit_memory(
                &mut state,
                context,
                accounted.saturating_sub(slot.ledgered_bytes()),
            ) {
                stage.activity = ActivityIndex::default();
                stage.indexed = 0;
                stage.committed = None;
                state.set_classifier_charge(&slot, stage.accounted_bytes());
                return Err(error);
            }
            state.set_classifier_charge(&slot, accounted);
        }
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
            let generation_key = snapshot_key(&derived);
            let anchors = || {
                generation
                    .anchors()
                    .chain(carried.iter().flat_map(|candidate| candidate.anchors()))
            };
            let additional = state.admission(&generation_key, &generation, anchors())
                + state.generations.growth_for(&generation_key)
                + state.classified_growth(&key, &derived)
                + carried.as_ref().map_or(0, |candidate| {
                    charged_bytes(&lineage, candidate) + state.carried_growth(&lineage, candidate)
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
            state.insert_classified(key, Arc::clone(&derived));
        }
        stage.activity = ActivityIndex::default();
        stage.committed = None;
        stage.result = Some(Arc::clone(&derived));
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
        let carried = Arc::clone(carried);
        state.touch_carried_owner(&carried);
        Some(carried)
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
                    if state.expired_prepared_queries.len() >= EXPIRED_QUERY_TOMBSTONES {
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
        state
            .registered_builds
            .retain(|_, build| build.expires > now);
        let expired_builds: Vec<_> = state
            .prepared_builds
            .iter()
            .filter(|(_, cursor)| cursor.expires <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired_builds {
            state.remove_prepared_build(&token);
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

    fn gauge_terms(state: &mut StoreState) -> GaugeTerms {
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
                + state.classified.charged()
                + state.carried_owners.charged()
                + state.catalog_owners.charged()
                + state.carried_classifications.charged()
                + state.leases.charged()
                + state.waiters.charged()
                + state.resolutions.charged()
                + state.warm_memberships.charged()
                + state.ledger.shared.warm()
                + state.locations.charged()
                + state.locates.charged()
                + state.catalogs.charged()
                + state.registered_builds.charged(),
            live_generations: state.generations.len(),
        }
    }

    fn retained_total(state: &mut StoreState) -> usize {
        let (_, _, total) = Self::gauge_terms(state).totals(state);
        total
    }

    fn gauges(state: &mut StoreState) -> Value {
        Self::gauge_terms(state).report(state)
    }

    #[cfg(test)]
    fn audit_gauges(state: &StoreState) -> Value {
        state.audits.fetch_add(1, Ordering::Relaxed);
        let records = Self::walked_records(state);
        let walked = |tables: &[&str]| {
            records
                .iter()
                .filter(|(table, _, _)| tables.contains(table))
                .map(|(_, _, bytes)| *bytes)
                .sum::<usize>()
        };
        let mut allocations = HashSet::new();
        let mut snapshots = HashSet::new();
        let mut entries = 0usize;
        let mut indexes = 0usize;
        for record in state.generations.values() {
            let Some(snapshot) = record.snapshot.upgrade() else {
                continue;
            };
            snapshots.insert(snapshot_key(&snapshot));
            for (id, bytes) in Self::audit_snapshot_allocations(&snapshot) {
                if allocations.insert(id) {
                    entries += bytes;
                }
            }
            for (id, bytes) in snapshot.activity.audited_allocations(true) {
                if allocations.insert(id) {
                    indexes += bytes;
                }
            }
            if let Some(window) = &record.raw_window {
                if allocations.insert(Arc::as_ptr(window) as usize) {
                    entries += arc_mirror::<crate::snapshot_raw_window::RawWindow>();
                }
                if let Some((id, bytes)) = window.audited_payload_allocation() {
                    if allocations.insert(id) {
                        entries += bytes;
                    }
                }
            }
        }
        for registry in state.registries.values() {
            for (id, bytes) in registry.snapshot.audited_allocations() {
                if allocations.insert(id) {
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
            if allocations.insert(Arc::as_ptr(carried) as usize) {
                indexes += arc_mirror::<CarriedClassification>()
                    + carried.prefix.capacity() * size_of::<Arc<EntryChunk>>();
            }
            for (id, bytes) in carried.activity.audited_allocations(true) {
                if allocations.insert(id) {
                    indexes += bytes;
                }
            }
        }
        for (id, (owner, _)) in state.escaped_chunks.iter() {
            let bytes = match owner {
                ChunkAllocationOwner::Rows(rows) => rows
                    .upgrade()
                    .map(|rows| Self::audit_chunk_rows_bytes(&rows)),
                ChunkAllocationOwner::Metadata(chunk) => chunk
                    .upgrade()
                    .map(|chunk| Self::audit_chunk_metadata_bytes(&chunk)),
            };
            if let Some(bytes) = bytes {
                if allocations.insert(*id) {
                    entries += bytes;
                }
            }
        }
        GaugeTerms {
            entries,
            indexes,
            pending: state
                .loads
                .values()
                .map(|slot| {
                    Self::audit_load_record_bytes(slot) + slot.accounted.load(Ordering::Acquire)
                })
                .sum(),
            classifier: state
                .classifier_stages
                .iter()
                .map(|(key, slot)| {
                    arc_mirror::<ClassifierSlot>()
                        + MUTEX_STORAGE_MIRROR
                        + slot.chunks.capacity() * size_of::<Arc<EntryChunk>>()
                        + key.capacity()
                        + slot.accounted.load(Ordering::Acquire)
                })
                .sum(),
            label: walked(&["labels"]),
            discovery: walked(&["discoveries", "checkpoints"]),
            projections: walked(&["projections"]),
            graph: walked(&[
                "graphs",
                "prepared_graphs",
                "prepared_queries",
                "prepared_builds",
                "expired_prepared_queries",
                "prepared_facts",
            ]) + Self::audit_prepared_fact_bytes(state),
            metadata: walked(&[
                "deliveries",
                "registries",
                "generations",
                "classified",
                "carried_owners",
                "catalog_owners",
                "carried_classifications",
                "leases",
                "waiters",
                "resolutions",
                "warm_memberships",
                "locations",
                "locates",
                "catalogs",
                "registered_builds",
            ]) + Self::audit_warm_buffer_bytes(state),
            live_generations: snapshots.len(),
        }
        .report(state)
    }

    #[cfg(test)]
    fn walked_records(state: &StoreState) -> [(&'static str, usize, usize); 25] {
        [
            (
                "catalog_owners",
                state.catalog_owners.charged(),
                state
                    .catalog_owners
                    .audit_with(|_, record| arc_slice_mirror::<u8>(record.key.len())),
            ),
            (
                "classified",
                state.classified.charged(),
                state.classified.audit_with(|key, _| key.capacity()),
            ),
            (
                "carried_owners",
                state.carried_owners.charged(),
                state.carried_owners.audit_with(|_, record| {
                    record.lineages.capacity() * size_of::<(SourceIdentity, String)>()
                        + record
                            .lineages
                            .iter()
                            .map(|(_, key)| key.capacity())
                            .sum::<usize>()
                }),
            ),
            (
                "labels",
                state.labels.charged(),
                state.labels.audit_with(Self::audit_label_bytes),
            ),
            (
                "discoveries",
                state.discoveries.charged(),
                state.discoveries.audit_with(Self::audit_discovery_bytes),
            ),
            (
                "checkpoints",
                state.checkpoints.charged(),
                state.checkpoints.audit_with(Self::audit_checkpoint_bytes),
            ),
            (
                "projections",
                state.projections.charged(),
                state.projections.audit_with(Self::audit_projection_bytes),
            ),
            (
                "graphs",
                state.graphs.charged(),
                state.graphs.audit_with(Self::audit_graph_cursor_bytes),
            ),
            (
                "prepared_graphs",
                state.prepared_graphs.charged(),
                state
                    .prepared_graphs
                    .audit_with(Self::audit_prepared_graph_bytes),
            ),
            (
                "prepared_queries",
                state.prepared_queries.charged(),
                state
                    .prepared_queries
                    .audit_with(Self::audit_prepared_query_bytes),
            ),
            (
                "prepared_builds",
                state.prepared_builds.charged(),
                state
                    .prepared_builds
                    .audit_with(Self::audit_prepared_build_bytes),
            ),
            (
                "expired_prepared_queries",
                state.expired_prepared_queries.charged(),
                state
                    .expired_prepared_queries
                    .audit_with(Self::audit_expired_query_bytes),
            ),
            (
                "prepared_facts",
                state.prepared_facts.charged(),
                state
                    .prepared_facts
                    .audit_with(Self::audit_cached_facts_bytes),
            ),
            (
                "deliveries",
                state.deliveries.charged(),
                state.deliveries.audit_with(Self::audit_delivery_bytes),
            ),
            (
                "registries",
                state.registries.charged(),
                state.registries.audit_with(Self::audit_registry_bytes),
            ),
            (
                "generations",
                state.generations.charged(),
                state.generations.audit_with(Self::audit_generation_bytes),
            ),
            (
                "carried_classifications",
                state.carried_classifications.charged(),
                state
                    .carried_classifications
                    .audit_with(Self::audit_carried_bytes),
            ),
            (
                "leases",
                state.leases.charged(),
                state.leases.audit_with(Self::audit_lease_bytes),
            ),
            (
                "waiters",
                state.waiters.charged(),
                state.waiters.audit_with(Self::audit_waiter_bytes),
            ),
            (
                "resolutions",
                state.resolutions.charged(),
                state.resolutions.audit_with(Self::audit_resolution_bytes),
            ),
            (
                "warm_memberships",
                state.warm_memberships.charged(),
                state
                    .warm_memberships
                    .audit_with(Self::audit_warm_membership_bytes),
            ),
            (
                "locations",
                state.locations.charged(),
                state.locations.audit_with(Self::audit_location_bytes),
            ),
            (
                "locates",
                state.locates.charged(),
                state.locates.audit_with(Self::audit_locate_bytes),
            ),
            (
                "catalogs",
                state.catalogs.charged(),
                state.catalogs.audit_with(Self::audit_catalog_bytes),
            ),
            (
                "registered_builds",
                state.registered_builds.charged(),
                state
                    .registered_builds
                    .audit_with(Self::audit_registered_build_bytes),
            ),
        ]
    }

    #[cfg(test)]
    fn audit_snapshot_allocations(snapshot: &Arc<TranscriptSnapshot>) -> Vec<(usize, usize)> {
        let started = if snapshot.window_start == 0 {
            None
        } else {
            snapshot
                .chunks
                .iter()
                .flat_map(|chunk| chunk.entries.iter())
                .find_map(Entry::meta)
                .map(|meta| meta.timestamp.timestamp_millis())
        };
        assert_eq!(snapshot.window_started_unix_ms(), started);
        let model = snapshot
            .chunks
            .iter()
            .rev()
            .flat_map(|chunk| chunk.entries.iter().rev())
            .find_map(|entry| match entry {
                Entry::Assistant(assistant) if assistant.model != SYNTHETIC_MODEL => {
                    Some(assistant.model.as_str())
                }
                _ => None,
            });
        assert_eq!(snapshot.model(), model);
        let TranscriptSnapshot {
            ledger: _,
            id,
            canonical_path,
            stamp: _,
            provider: _,
            session_id,
            chunks,
            activity: _,
            window_start: _,
            committed_bytes: _,
            provisional_tail: _,
            fence,
            event_count: _,
            codex_raw,
            codex_append,
        } = &**snapshot;
        std::iter::once((
            snapshot_key(snapshot),
            arc_mirror::<TranscriptSnapshot>()
                + id.capacity()
                + canonical_path.capacity()
                + session_id.capacity()
                + chunks.capacity() * size_of::<Arc<EntryChunk>>()
                + fence.capacity(),
        ))
        .chain(chunks.iter().flat_map(|chunk| {
            let metadata = arc_mirror::<EntryChunk>()
                + chunk.entry_charges.capacity() * size_of::<MemoryCharge>();
            [
                (Arc::as_ptr(chunk) as usize, metadata),
                (
                    Arc::as_ptr(&chunk.entries) as usize,
                    Self::audit_chunk_bytes(chunk) - metadata,
                ),
            ]
        }))
        .chain(codex_raw.iter().map(|raw| {
            (
                Arc::as_ptr(raw) as usize,
                arc_mirror::<Vec<u8>>() + raw.capacity(),
            )
        }))
        .chain(codex_append.iter().map(|index| {
            (
                Arc::as_ptr(index) as usize,
                Self::audit_codex_index_bytes(index),
            )
        }))
        .collect()
    }

    #[cfg(test)]
    fn audit_chunk_bytes(chunk: &EntryChunk) -> usize {
        Self::audit_chunk_metadata_bytes(chunk) + Self::audit_chunk_rows_bytes(&chunk.entries)
    }

    #[cfg(test)]
    fn audit_chunk_metadata_bytes(chunk: &EntryChunk) -> usize {
        arc_mirror::<EntryChunk>() + chunk.entry_charges.capacity() * size_of::<MemoryCharge>()
    }

    #[cfg(test)]
    fn audit_chunk_rows_bytes(rows: &ChunkRows) -> usize {
        arc_mirror::<ChunkRows>()
            + rows.capacity() * size_of::<Entry>()
            + rows.arenas().capacity() * size_of::<(usize, SourceArena)>()
            + rows
                .iter()
                .map(|entry| {
                    let charge = entry_charge(entry);
                    charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                })
                .sum::<usize>()
            + rows
                .arenas()
                .iter()
                .map(|(_, arena)| Self::audit_arena_bytes(arena))
                .sum::<usize>()
    }

    #[cfg(test)]
    fn audit_arena_bytes(arena: &SourceArena) -> usize {
        64 + arena.source_len + 64 + (32 + Self::audit_arena_nodes(&arena.root)).max(448) + 48
    }

    #[cfg(test)]
    fn audit_arena_nodes(value: &Value) -> usize {
        if let Some(array) = value.as_array().filter(|array| !array.is_empty()) {
            16 * (array.len() + 1) + array.iter().map(Self::audit_arena_nodes).sum::<usize>()
        } else if let Some(object) = value.as_object().filter(|object| !object.is_empty()) {
            16 * (2 * object.len() + 1)
                + object
                    .iter()
                    .map(|(_, item)| Self::audit_arena_nodes(item))
                    .sum::<usize>()
        } else {
            0
        }
    }

    #[cfg(test)]
    fn audit_codex_index_bytes(index: &CodexAppendIndex) -> usize {
        let CodexAppendIndex {
            thread_id,
            cwd,
            model,
            lines: _,
            terminated: _,
            last_trigger_turn: _,
            user_echoes,
            assistant_echoes,
            user_events,
            assistant_events,
        } = index;
        arc_mirror::<CodexAppendIndex>()
            + thread_id.as_ref().map_or(0, String::capacity)
            + cwd.as_ref().map_or(0, String::capacity)
            + model.as_ref().map_or(0, String::capacity)
            + user_echoes.capacity() * size_of::<String>()
            + user_echoes.iter().map(String::capacity).sum::<usize>()
            + assistant_echoes.capacity() * size_of::<String>()
            + assistant_echoes.iter().map(String::capacity).sum::<usize>()
            + (user_events.capacity() + assistant_events.capacity()) * size_of::<[u8; 32]>()
    }

    #[cfg(test)]
    fn audit_load_record_bytes(slot: &LoadSlot) -> usize {
        let LoadSlot {
            id,
            path,
            stamp: _,
            registry_generation,
            registry: _,
            work: _,
            accounted: _,
            attached: _,
            deadline: _,
        } = slot;
        arc_mirror::<LoadSlot>()
            + MUTEX_STORAGE_MIRROR
            + id.capacity()
            + path.capacity()
            + registry_generation.capacity()
    }

    #[cfg(test)]
    fn audit_open_directory_bytes(directory: &OpenDirectory) -> usize {
        let OpenDirectory {
            entries,
            end_of_stream: _,
        } = directory;
        let OpenedDirectory {
            directory: _,
            root,
            stamp: _,
            parent_metadata_attempts: _,
        } = entries.as_ref();
        arc_mirror::<(*mut libc::DIR, PathBuf, SourceStamp, AtomicUsize)>() + root.capacity()
    }

    #[cfg(test)]
    fn audit_lease_bytes(token: &String, lease: &Lease) -> usize {
        let Lease {
            claimant,
            snapshot: _,
            classifier,
            expires: _,
            absolute_deadline: _,
            exposed: _,
            delivery,
            registry_generation,
            registry: _,
        } = lease;
        token.capacity()
            + claimant.capacity()
            + value_bytes(classifier)
            + *delivery
            + registry_generation.capacity()
    }

    #[cfg(test)]
    fn audit_waiter_bytes(token: &String, waiter: &Waiter) -> usize {
        let Waiter {
            claimant,
            load: _,
            classifier,
            context,
            limits: _,
            created: _,
            expires: _,
            deadline: _,
            used_bytes: _,
            used_source_bytes: _,
            used_events: _,
            stage: _,
            windowed: _,
            busy: _,
        } = waiter;
        token.capacity() + claimant.capacity() + value_bytes(classifier) + value_bytes(context)
    }

    #[cfg(test)]
    fn audit_projection_bytes(token: &String, cursor: &ProjectionCursor) -> usize {
        let ProjectionCursor {
            claimant,
            registry_generation,
            admission,
            request,
            limits: _,
            next: _,
            expires: _,
        } = cursor;
        token.capacity()
            + claimant.capacity()
            + registry_generation.capacity()
            + admission.capacity()
            + value_bytes(request)
    }

    #[cfg(test)]
    fn audit_generation_bytes(_: &usize, record: &GenerationRecord) -> usize {
        let GenerationRecord {
            snapshot: _,
            cached,
            registry_generation,
            entries,
            indexes,
            source_chunks,
            raw_window: _,
            raw_parent: _,
            raw_cache,
            interactions,
        } = record;
        source_chunks.as_ref().map_or(0, |chunks| {
            arc_mirror::<Vec<SourceChunkSpan>>() + chunks.capacity() * size_of::<SourceChunkSpan>()
        }) + registry_generation.capacity()
            + cached.as_ref().map_or(0, |owner| {
                owner.classified.as_ref().map_or(0, String::capacity)
            })
            + entries.capacity() * size_of::<(usize, MemoryCharge)>()
            + indexes.capacity() * size_of::<(usize, usize)>()
            + raw_cache.as_ref().map_or(0, |cache| {
                let RawCapabilityCache {
                    stamp: _,
                    tail_bytes: _,
                    raw: _,
                    excerpt,
                    tail,
                } = cache.as_ref();
                size_of::<RawCapabilityCache>()
                    + excerpt.as_ref().map_or(0, |scope| scope.key.capacity())
                    + tail.as_ref().map_or(0, |scope| scope.key.capacity())
            })
            + interactions
                .as_ref()
                .map_or(0, |index| index.audited_bytes())
    }

    #[cfg(test)]
    fn audit_carried_bytes(
        (_, lineage): &(SourceIdentity, String),
        _: &Arc<CarriedClassification>,
    ) -> usize {
        lineage.capacity()
    }

    #[cfg(test)]
    fn audit_graph_cursor_bytes(token: &String, graph: &GraphCursor) -> usize {
        let GraphCursor {
            claimant,
            context,
            request,
            root_handle,
            remaining: _,
            nodes,
            seen,
            tasks,
            listing,
            pending,
            prepared: _,
            root_checked: _,
            projection_at: _,
            pending_records,
            published_members,
            expires: _,
        } = graph;
        let node = |GraphNode {
                        path,
                        depth: _,
                        spawned_by,
                        snapshot: _,
                        description,
                        transferred: _,
                    }: &GraphNode| {
            path.capacity()
                + spawned_by.as_ref().map_or(0, String::capacity)
                + value_bytes(description)
        };
        let task = |task: &GraphTask| match task {
            GraphTask::Visit {
                path,
                depth: _,
                spawned_by,
            } => path.capacity() + spawned_by.as_ref().map_or(0, String::capacity),
            GraphTask::List { parent, depth: _ } => parent.capacity(),
        };
        token.capacity()
            + size_of::<GraphCursor>()
            + claimant.capacity()
            + value_bytes(context)
            + value_bytes(request)
            + value_bytes(root_handle)
            + nodes.capacity() * size_of::<GraphNode>()
            + nodes.iter().map(node).sum::<usize>()
            + seen.capacity() * size_of::<SourceIdentity>()
            + tasks.capacity() * size_of::<GraphTask>()
            + tasks.iter().map(task).sum::<usize>()
            + listing.as_ref().map_or(
                0,
                |GraphListing {
                     entries,
                     children,
                     depth: _,
                 }| {
                    children.capacity() * size_of::<PathBuf>()
                        + children.iter().map(PathBuf::capacity).sum::<usize>()
                        + Self::audit_open_directory_bytes(entries)
                },
            )
            + pending.as_ref().map_or(
                0,
                |GraphPending {
                     token,
                     path,
                     depth: _,
                     spawned_by,
                 }| {
                    token.capacity()
                        + path.capacity()
                        + spawned_by.as_ref().map_or(0, String::capacity)
                },
            )
            + pending_records.capacity() * size_of::<(usize, String)>()
            + pending_records
                .iter()
                .map(|(_, record)| record.capacity())
                .sum::<usize>()
            + published_members.capacity() * size_of::<usize>()
    }

    #[cfg(test)]
    fn audit_prepared_query_bytes(token: &String, cursor: &PreparedQueryCursor) -> usize {
        let PreparedQueryCursor {
            claimant,
            graph_id,
            query,
            pending,
            input_records,
            observers:
                Observers {
                    declared: _,
                    allowance: _,
                    witnessed: _,
                    pending: observing,
                },
            need: _,
            next: _,
            page_output_bytes: _,
            remaining: _,
            expires: _,
        } = cursor;
        token.capacity()
            + size_of::<PreparedQueryCursor>()
            + claimant.capacity()
            + graph_id.capacity()
            + value_bytes(query)
            + pending.as_ref().map_or(
                0,
                |PendingPreparedSource {
                     token,
                     path,
                     stamp: _,
                 }| token.capacity() + path.capacity(),
            )
            + input_records.as_ref().map_or(0, |records| {
                records.capacity() * size_of::<String>()
                    + records.iter().map(String::capacity).sum::<usize>()
            })
            + observing.capacity() * size_of::<(usize, Value)>()
            + observing
                .iter()
                .map(|(_, query)| value_bytes(query))
                .sum::<usize>()
    }

    #[cfg(test)]
    fn audit_expired_query_bytes(token: &String, (claimant, _): &(String, u64)) -> usize {
        token.capacity() + claimant.capacity()
    }

    #[cfg(test)]
    fn audit_cached_facts_bytes(_: &PreparedDiskKey, cached: &CachedPreparedFacts) -> usize {
        let CachedPreparedFacts {
            stamp: _,
            registry_generation,
            admission,
            authority,
            classifier,
            facts: _,
            last_used: _,
        } = cached;
        registry_generation.capacity()
            + admission.capacity()
            + value_bytes(authority)
            + value_bytes(classifier)
    }

    #[cfg(test)]
    fn audit_warm_membership_bytes(key: &String, membership: &WarmMembership) -> usize {
        let WarmMembership {
            catalog: _,
            members: _,
            positions: _,
            sidechain_dirs: _,
            revision,
            complete: _,
            expires: _,
        } = membership;
        key.capacity() + size_of::<WarmMembership>() + revision.capacity()
    }

    #[cfg(test)]
    fn audit_delivery_bytes(key: &Arc<str>, delivery: &Delivery) -> usize {
        let Delivery {
            claimant,
            leases,
            cursor,
            expires: _,
        } = delivery;
        arc_slice_mirror::<u8>(key.len())
            + size_of::<Delivery>()
            + claimant.capacity()
            + leases.capacity() * size_of::<String>()
            + leases.iter().map(String::capacity).sum::<usize>()
            + cursor.as_ref().map_or(0, String::capacity)
    }

    #[cfg(test)]
    fn audit_label_bytes(token: &String, slot: &LabelSlot) -> usize {
        let LabelSlot {
            preparation,
            source_handle,
            accounted: _,
            expires: _,
            admission,
        } = slot;
        token.capacity()
            + size_of::<LabelSlot>()
            + preparation.audited_bytes()
            + value_bytes(source_handle)
            + admission.capacity()
    }

    #[cfg(test)]
    fn audit_registry_bytes(fingerprint: &String, record: &RegistryRecord) -> usize {
        let RegistryRecord {
            snapshot: _,
            allocations,
        } = record;
        fingerprint.capacity() + allocations.capacity() * size_of::<(usize, usize)>()
    }

    #[cfg(test)]
    fn audit_discovery_bytes(token: &String, scan: &DiscoveryCursor) -> usize {
        let DiscoveryCursor {
            claimant,
            request,
            context,
            limits: _,
            roots,
            directories,
            seen,
            seen_directories,
            examined: _,
            sources: _,
            emitted: _,
            output_bytes: _,
            inventory,
            inventory_payload: _,
            previous,
            removed,
            walking: _,
            expires: _,
        } = scan;
        let table = |map: &HashMap<String, Value>| {
            map.capacity() * size_of::<(String, Value)>()
                + map
                    .iter()
                    .map(|(path, value)| path.capacity() + value_bytes(value))
                    .sum::<usize>()
        };
        token.capacity()
            + claimant.capacity()
            + value_bytes(request)
            + value_bytes(context)
            + roots.capacity() * size_of::<PathBuf>()
            + roots.iter().map(PathBuf::capacity).sum::<usize>()
            + directories.capacity() * size_of::<OpenDirectory>()
            + directories
                .iter()
                .map(Self::audit_open_directory_bytes)
                .sum::<usize>()
            + seen.capacity() * size_of::<SourceIdentity>()
            + seen_directories.capacity() * size_of::<SourceIdentity>()
            + table(inventory)
            + table(previous)
            + removed.capacity() * size_of::<Value>()
            + removed.iter().map(value_bytes).sum::<usize>()
    }

    #[cfg(test)]
    fn audit_checkpoint_bytes(token: &String, checkpoint: &Checkpoint) -> usize {
        let Checkpoint {
            claimant,
            roots,
            inventory,
            expires: _,
        } = checkpoint;
        token.capacity()
            + claimant.capacity()
            + value_bytes(roots)
            + inventory.capacity() * size_of::<(String, Value)>()
            + inventory
                .iter()
                .map(|(path, value)| path.capacity() + value_bytes(value))
                .sum::<usize>()
    }

    #[cfg(test)]
    fn audit_resolution_bytes(token: &String, cursor: &ResolutionCursor) -> usize {
        let ResolutionCursor {
            claimant,
            context,
            request,
            ids,
            paths,
            sessions,
            next: _,
            pending,
            remaining: _,
            complete_scan: _,
            expires: _,
        } = cursor;
        token.capacity()
            + claimant.capacity()
            + value_bytes(context)
            + value_bytes(request)
            + ids.capacity() * size_of::<String>()
            + ids.iter().map(String::capacity).sum::<usize>()
            + paths.capacity() * size_of::<(String, PathBuf)>()
            + paths
                .iter()
                .map(|(id, path)| id.capacity() + path.capacity())
                .sum::<usize>()
            + sessions.capacity() * size_of::<Value>()
            + sessions.iter().map(value_bytes).sum::<usize>()
            + pending.as_ref().map_or(0, String::capacity)
    }

    #[cfg(test)]
    fn audit_location_bytes(id: &String, location: &LocatedPath) -> usize {
        let LocatedPath { path, expires: _ } = location;
        id.capacity() + path.capacity()
    }

    #[cfg(test)]
    fn audit_locate_bytes(token: &String, cursor: &LocateCursor) -> usize {
        let LocateCursor {
            claimant,
            context,
            limits: _,
            ids,
            wanted,
            found,
            scope,
            roots,
            directories,
            seen_directories,
            pending,
            examined: _,
            emitted: _,
            output_bytes: _,
            finished: _,
            exhausted: _,
            expires: _,
        } = cursor;
        token.capacity()
            + size_of::<LocateCursor>()
            + claimant.capacity()
            + value_bytes(context)
            + ids.capacity() * size_of::<String>()
            + ids.iter().map(String::capacity).sum::<usize>()
            + wanted.capacity() * size_of::<String>()
            + wanted.iter().map(String::capacity).sum::<usize>()
            + found.capacity() * size_of::<String>()
            + found.iter().map(String::capacity).sum::<usize>()
            + scope.capacity() * size_of::<PathBuf>()
            + scope.iter().map(PathBuf::capacity).sum::<usize>()
            + roots.capacity() * size_of::<PathBuf>()
            + roots.iter().map(PathBuf::capacity).sum::<usize>()
            + directories.capacity() * size_of::<OpenDirectory>()
            + directories
                .iter()
                .map(Self::audit_open_directory_bytes)
                .sum::<usize>()
            + seen_directories.capacity() * size_of::<SourceIdentity>()
            + pending.capacity() * size_of::<Value>()
            + pending.iter().map(value_bytes).sum::<usize>()
    }

    fn fixed_metadata_bytes(state: &StoreState) -> usize {
        Self::bookkeeping_bytes(state) + RELEASE_QUEUE_BYTES + state.ledger.queue.buffer_bytes()
    }

    fn bookkeeping_bytes(state: &StoreState) -> usize {
        size_of::<StoreState>()
            + state.ledger.shared.table_bytes()
            + state.registries.capacity_bytes()
            + state.generations.capacity_bytes()
            + state.carried_classifications.capacity_bytes()
            + state.leases.capacity_bytes()
            + state.waiters.capacity_bytes()
            + state.deliveries.capacity_bytes()
            + state.projections.capacity_bytes()
            + state.graphs.capacity_bytes()
            + state.prepared_graphs.capacity_bytes()
            + state.prepared_builds.capacity_bytes()
            + state.prepared_queries.capacity_bytes()
            + state.expired_prepared_queries.capacity_bytes()
            + state.prepared_facts.capacity_bytes()
            + state.labels.capacity_bytes()
            + state.discoveries.capacity_bytes()
            + state.checkpoints.capacity_bytes()
            + state.resolutions.capacity_bytes()
            + state.loads.reserved_bytes()
            + state.classifier_stages.reserved_bytes()
            + state.latest.reserved_bytes()
            + state.source_spines.reserved_bytes()
            + state.classified.capacity_bytes()
            + state.carried_owners.capacity_bytes()
            + state.catalog_owners.capacity_bytes()
            + state.cache_order.len() * size_of::<(u64, CacheOwner)>()
            + state.source_view_nodes * size_of::<SourceView>()
            + state.escaped_chunks.reserved_bytes()
            + state.prepared_loads.reserved_bytes()
            + state.recent_codex.reserved_bytes()
            + state.warm_memberships.capacity_bytes()
            + state.locations.capacity_bytes()
            + state.locates.capacity_bytes()
            + state.catalogs.capacity_bytes()
            + state.registered_builds.capacity_bytes()
            + state.locations_expiry.heap_bytes()
            + state.carried_expiry.heap_bytes()
            + state.recent_codex_expiry.heap_bytes()
            + state.prepared_loads_expiry.heap_bytes()
            + state.deliveries_expiry.index_bytes()
            + state.prepared_facts_lru.len() * size_of::<(u64, PreparedDiskKey)>()
            + state.prepared_disk_index_bytes
    }

    #[cfg(test)]
    fn audit_recent_codex_raw_bytes(state: &StoreState) -> usize {
        state
            .recent_codex
            .keys()
            .map(|identity| state.latest_raw_bytes(identity))
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn retained_accounted_bytes(&self) -> usize {
        let mut state = self.lock_state();
        Self::retained_total(&mut state)
    }

    #[cfg(test)]
    pub(crate) fn assert_conserved(&self) {
        let disk_index = self.prepared_disk.audit_index_bytes();
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
        for (table, charged, walked) in Self::walked_records(&state) {
            assert_eq!(
                walked, charged,
                "{table} records diverge from their field walk"
            );
        }
        for record in state.generations.values() {
            let Some(snapshot) = record.snapshot.upgrade() else {
                continue;
            };
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
                Self::audit_snapshot_allocations(&snapshot),
                "a generation's entry records diverge from their snapshot walk"
            );
            assert_eq!(
                record.indexes,
                snapshot.activity.audited_allocations(true),
                "a generation's index records diverge from their activity walk"
            );
        }
        assert_eq!(
            state.prepared_disk_index_bytes, disk_index,
            "prepared disk index ledger diverges from the index tier"
        );
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
        for (key, cached) in state.prepared_facts.iter() {
            assert!(
                state.prepared_facts_lru.contains(&(cached.last_used, *key)),
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
        state.assert_cache_owners();
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
        if Self::retained_total(state).saturating_add(additional) <= cap {
            state.ledger.shared.work().admitted(additional);
            return Ok(());
        }
        while Self::retained_total(state).saturating_add(additional) > cap {
            let Some(owner) = state.next_cache_victim() else {
                break;
            };
            state.evict_cache_owner(owner);
        }
        if Self::retained_total(state).saturating_add(additional) > cap {
            debug_assert!(
                bookkeeping.is_some_and(|before| Self::bookkeeping_bytes(state) <= before),
                "a refused admission grew retained bookkeeping"
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

    fn rebind_waiter(
        &self,
        state: &mut StoreState,
        token: &str,
        context: &Value,
    ) -> Result<Option<Waiter>, SnapshotError> {
        let Some(current) = state.waiters.get(token) else {
            return Ok(None);
        };
        if current.busy {
            return Ok(Some(current.clone()));
        }
        let growth = value_bytes(context).saturating_sub(value_bytes(&current.context));
        if growth > 0 {
            self.admit_memory(state, context, growth)?;
        }
        let mut waiter = state.waiters.get_mut(token).expect("rebound waiter");
        waiter.context = context.clone();
        Ok(Some(Waiter::clone(&waiter)))
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
        let claimant = str_field(context, "claimant")?;
        let lease = Lease {
            claimant: claimant.to_owned(),
            snapshot,
            classifier,
            expires,
            absolute_deadline,
            exposed: false,
            delivery: Delivery::lease_pledge(claimant, &token),
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
        #[cfg(test)]
        self.description_builds.fetch_add(1, Ordering::Relaxed);
        json!({"handle": LeaseHandle {owner_epoch: &self.owner_epoch, snapshot_id: &snapshot.id, generation: &snapshot.id, lease_id: lease},
            "canonical_path": snapshot.canonical_path.to_string_lossy().as_ref(),
            "source_id": format!("{}:{}", snapshot.stamp.identity.device, snapshot.stamp.identity.inode),
            "device": snapshot.stamp.identity.device.to_string(), "inode": snapshot.stamp.identity.inode.to_string(),
            "mtime_ns": snapshot.stamp.mtime_ns.to_string(), "ctime_ns": snapshot.stamp.ctime_ns.to_string(),
            "provider": snapshot.provider.as_str(), "parser_version": PARSER_VERSION,
            "source_bytes": snapshot.stamp.size, "window_start": snapshot.window_start, "window_started_unix_ms": snapshot.window_started_unix_ms(), "model": snapshot.model(), "committed_bytes": snapshot.committed_bytes,
            "event_count": snapshot.event_count, "turn_count": snapshot.activity.turn_count(),
            "classifier": classifier, "provisional_tail": snapshot.provisional_tail,"lease_expires_unix_ms":expires})
    }

    pub fn pin(
        &self,
        handle: &Value,
        context: &Value,
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        self.pin_record(handle, context).map(|scope| scope.snapshot)
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
        let scope = self.capture_scope(handle, context)?;
        let description = self.description(
            &scope.snapshot,
            &scope.classifier,
            str_field(handle, "lease_id")?,
            scope.expires,
        );
        self.authority(context, Some(&scope.snapshot.canonical_path))?;
        Ok((scope.snapshot, description))
    }

    fn pin_record(&self, handle: &Value, context: &Value) -> Result<LeasedScope, SnapshotError> {
        let scope = self.capture_scope(handle, context)?;
        self.authority(context, Some(&scope.snapshot.canonical_path))?;
        Ok(scope)
    }

    fn capture_scope(&self, handle: &Value, context: &Value) -> Result<LeasedScope, SnapshotError> {
        self.authority(context, None)?;
        let scope = {
            let mut state = self.lock_state();
            Self::prune(&mut state);
            let lease = self.lease(&state, handle, context)?;
            LeasedScope {
                snapshot: Arc::clone(&lease.snapshot),
                classifier: lease.classifier.clone(),
                expires: lease.expires,
            }
        };
        Ok(scope)
    }

    pub fn pin_scope_for_work(
        &self,
        handle: &Value,
        context: &Value,
        deadline: u64,
    ) -> Result<(Arc<TranscriptSnapshot>, Value), SnapshotError> {
        let scope = self.renew_scope_for_work(handle, context, deadline)?;
        let description = self.description(
            &scope.snapshot,
            &scope.classifier,
            str_field(handle, "lease_id")?,
            scope.expires,
        );
        Ok((scope.snapshot, description))
    }

    fn renew_scope_for_work(
        &self,
        handle: &Value,
        context: &Value,
        deadline: u64,
    ) -> Result<LeasedScope, SnapshotError> {
        #[cfg(test)]
        {
            let hook = self.pin_hook.lock().expect("pin hook").clone();
            if let Some(hook) = hook {
                if let Err(error) = hook(handle) {
                    self.pin_hook.lock().expect("pin hook").take();
                    return Err(error);
                }
            }
        }
        if deadline <= now_ms() {
            return Err(SnapshotError::new(
                Status::Deadline,
                "borrow deadline expired",
            ));
        }
        let scope = self.pin_record(handle, context)?;
        let mut state = self.lock_state();
        let lease = self.lease(&state, handle, context)?;
        let expires = lease.expires.max(deadline.min(lease.absolute_deadline));
        let classifier = lease.classifier.clone();
        state
            .leases
            .get_mut(str_field(handle, "lease_id")?)
            .expect("validated lease")
            .expires = expires;
        Ok(LeasedScope {
            snapshot: scope.snapshot,
            classifier,
            expires,
        })
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
        let mut usage = [0u64; 20];
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
                    if self.track_delivery(&pending,context,request.get("operation").and_then(Value::as_str)!=Some("describe")).is_ok() {
                        self.discard_response(&pending,context)?;
                    }
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
        if let Err(error) = self.track_delivery(
            &response,
            context,
            request.get("operation").and_then(Value::as_str) != Some("describe"),
        ) {
            usage[12] += 1;
            response = json!({"schema":SCHEMA,"id":id,"status":error.status.as_str(),"complete":false,"data":null,"cursor":null,"reason":error.reason,"usage":usage_value(&usage)});
        }
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
            Some("acquired_views") => {
                if let Some(views) = data["views"].as_array() {
                    for view in views.iter() {
                        add(&view["description"]);
                    }
                }
            }
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

    pub(crate) fn track_delivery(
        &self,
        response: &Value,
        context: &Value,
        may_issue: bool,
    ) -> Result<(), SnapshotError> {
        let Ok(handles) = Self::response_handles(response) else {
            return Ok(());
        };
        let Ok(key) = self.delivery_key(response, context, &handles) else {
            return Ok(());
        };
        let Ok(claimant) = str_field(context, "claimant") else {
            return Ok(());
        };
        let cursor = Self::response_cursor(response).map(str::to_owned);
        let mut state = self.lock_state();
        let mut leases: Vec<String> = if may_issue {
            handles
                .into_iter()
                .filter(|handle| {
                    state
                        .leases
                        .get(handle)
                        .is_some_and(|lease| lease.claimant == claimant && !lease.exposed)
                })
                .collect()
        } else {
            Vec::new()
        };
        leases.shrink_to_fit();
        if leases.is_empty() && cursor.is_none() {
            state.remove_delivery(&key);
            return Ok(());
        }
        let lease_pledges: usize = leases
            .iter()
            .map(|token| state.leases[token].delivery)
            .sum();
        let cursor_pledge = cursor
            .as_deref()
            .map_or(0, |cursor| state.consume_delivery_pledge(cursor));
        let pledged = lease_pledges + cursor_pledge;
        let key: Arc<str> = Arc::from(key);
        let delivery = Delivery {
            claimant: claimant.to_owned(),
            leases,
            cursor,
            expires: now_ms() + self.config.ttl,
        };
        let replaced = state
            .deliveries
            .get(&key)
            .map_or(0, |existing| Delivery::record_bytes(&key, existing));
        let additional = Delivery::record_bytes(&key, &delivery).saturating_sub(pledged + replaced);
        if additional > 0 {
            if let Err(error) = self.admit_memory(&mut state, context, additional) {
                for token in &delivery.leases {
                    state.leases.remove(token);
                    state.owned.release_lease(token);
                }
                drop(state);
                if let Some(cursor) = &delivery.cursor {
                    self.release_cursor(cursor, context)?;
                }
                return Err(error);
            }
        }
        for token in &delivery.leases {
            let mut lease = state.leases.get_mut(token).expect("exposed lease");
            lease.exposed = true;
            lease.delivery = 0;
        }
        if state.deliveries.len() >= self.config.leases.saturating_mul(4) {
            state.evict_earliest_delivery();
        }
        state.insert_delivery(key, delivery);
        Ok(())
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
            .registered_builds
            .get(token)
            .is_some_and(|build| build.claimant != claimant)
            || state
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
        released |= state.registered_builds.remove(token).is_some();
        released |= state.projections.remove(token).is_some();
        if let Some(query) = state.prepared_queries.remove(token) {
            if let Some(pending) = query.pending {
                state.waiters.remove(&pending.token);
            }
            released = true;
        }
        released |= state.remove_prepared_build(token).is_some();
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
            .registered_builds
            .get(token)
            .is_some_and(|build| build.claimant == claimant)
        {
            state.registered_builds.remove(token);
        }
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
        usage: &mut [u64; 20],
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
            "acquire_views" => self.acquire_views(request, context, cancel, usage),
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
                let waiter = {
                    let mut state = self.lock_state();
                    Self::prune(&mut state);
                    state.waiters.get(cursor).cloned()
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
                    state.discoveries.remove(cursor).map(|discovery| {
                        let held = discovery.charge();
                        state.transient_bytes += held;
                        (
                            discovery,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((mut discovery, mut reservation)) = discovery {
                    if let Err(error) = self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context) + LOCATE_PATH_SLOTS,
                    ) {
                        drop(discovery);
                        return Err(error);
                    }
                    discovery.context = context.clone();
                    return self.scan(cursor, discovery, &mut reservation, cancel, usage);
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
                    state.resolutions.remove(cursor).map(|resolution| {
                        let held = resolution.charge();
                        state.transient_bytes += held;
                        (
                            resolution,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((mut resolution, mut reservation)) = resolution {
                    if let Err(error) = self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context),
                    ) {
                        if let Some(pending) = &resolution.pending {
                            let mut state = self.lock_state();
                            state.waiters.remove(pending);
                            Self::prune(&mut state);
                        }
                        drop(resolution);
                        return Err(error);
                    }
                    resolution.context = context.clone();
                    return self.resolve_step(cursor, resolution, &mut reservation, cancel, usage);
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
                    state.locates.remove(cursor).map(|locate| {
                        let held = locate.charge();
                        state.transient_bytes += held;
                        (
                            locate,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((mut locate, mut reservation)) = locate {
                    if let Err(error) = self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context)
                            + if context["authority"]["kind"].as_str() == Some("user") {
                                LOCATE_USER_PATH_SLOTS
                            } else {
                                LOCATE_PATH_SLOTS
                            },
                    ) {
                        drop(locate);
                        return Err(error);
                    }
                    locate.context = context.clone();
                    return self.locate_step(cursor, locate, &mut reservation, cancel, usage);
                }
                let registered_build = {
                    let mut state = self.lock_state();
                    if state.registered_builds.get(cursor).is_some_and(|build| {
                        build.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared build claimant differs",
                        ));
                    }
                    state.registered_builds.remove(cursor).map(|build| {
                        let held = build.charge();
                        state.transient_bytes += held;
                        (
                            build,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((mut build, mut reservation)) = registered_build {
                    if build.context["authority"] != context["authority"]
                        || build.context["admission"] != context["admission"]
                        || build.context["registry_generation"] != context["registry_generation"]
                    {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared build context differs",
                        ));
                    }
                    self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context),
                    )?;
                    build.context = context.clone();
                    build.context_bytes = value_bytes(&build.context);
                    return self.registered_graph_step(
                        cursor,
                        build,
                        &mut reservation,
                        cancel,
                        usage,
                    );
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
                    state.prepared_builds.remove(cursor).map(|build| {
                        let held = build.charge();
                        state.transient_bytes += held;
                        (
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                            state.retained_facts(self, Arc::clone(&build.root_facts)),
                            build,
                        )
                    })
                };
                if let Some((mut reservation, _retained, mut build)) = prepared_build {
                    self.after_facts_returned();
                    if build.context["authority"] != context["authority"]
                        || build.context["admission"] != context["admission"]
                        || build.context["registry_generation"] != context["registry_generation"]
                    {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "prepared build context differs",
                        ));
                    }
                    self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context),
                    )?;
                    let context = context.clone();
                    let context_bytes = value_bytes(&context);
                    build.owned_payload_bytes =
                        build.owned_payload_bytes - build.context_bytes + context_bytes;
                    build.context_bytes = context_bytes;
                    build.context = context;
                    return self.prepare_graph_step(cursor, build, &mut reservation, cancel, usage);
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
                    state.prepared_queries.remove(cursor).map(|query| {
                        let held = query.charge();
                        state.transient_bytes += held;
                        (
                            query,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((query, mut reservation)) = prepared_query {
                    return self.prepared_query_page(
                        cursor,
                        query,
                        context,
                        &mut reservation,
                        cancel,
                        usage,
                    );
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
                    state.graphs.remove(cursor).map(|graph| {
                        let held = graph.charge();
                        state.transient_bytes += held;
                        (
                            graph,
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                        )
                    })
                };
                if let Some((mut graph, mut reservation)) = graph {
                    if let Err(error) = self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        value_bytes(context),
                    ) {
                        Self::rollback_graph_page(&mut self.lock_state(), &mut graph);
                        drop(graph);
                        return Err(error);
                    }
                    graph.context = context.clone();
                    return self.graph_step(cursor, graph, &mut reservation, cancel, usage);
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
                    self.rebind_waiter(&mut self.lock_state(), cursor, context)?;
                    return self.advance(cursor, waiter, None, cancel, usage);
                }
                let projection = {
                    let mut state = self.lock_state();
                    if state.projections.get(cursor).is_some_and(|projection| {
                        projection.claimant != str_field(context, "claimant").unwrap_or("")
                    }) {
                        return Err(SnapshotError::new(
                            Status::StaleCursor,
                            "cursor claimant differs",
                        ));
                    }
                    state.projections.remove(cursor).map(|projection| {
                        let held = projection.charge();
                        state.transient_bytes += held;
                        (
                            ProjectionReservation {
                                store: self,
                                bytes: held,
                            },
                            projection,
                        )
                    })
                };
                if let Some((mut reservation, projection)) = projection {
                    let ProjectionCursor {
                        request,
                        limits,
                        next,
                        ..
                    } = projection;
                    return self.project_request(
                        Cow::Owned(request),
                        context,
                        cancel,
                        limits,
                        next,
                        &mut reservation,
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
                    json!({"kind": "stats", "counters": usage_value(&self.reported_counters(&state)), "gauges": Self::gauges(&mut state)}),
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
            "query" | "capture" | "activity_probe" | "hydrate" | "mine" | "classify_view" => {
                let bound = limits(request)?;
                let mut reservation = ProjectionReservation {
                    store: self,
                    bytes: 0,
                };
                self.project_request(
                    Cow::Borrowed(request),
                    context,
                    cancel,
                    bound,
                    0,
                    &mut reservation,
                )
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
        usage: &mut [u64; 20],
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
        let budget = limits
            .max_source_read_bytes
            .min(self.config.source)
            .min(stamp.size as usize);
        let mut reservation = self.reserve_projection(context, 0)?;
        let mut pending: Vec<u8> = Vec::new();
        let mut pending_start = stamp.size;
        let mut cut = 0usize;
        let mut read = 0usize;
        let mut provider = None;
        let mut lines: Vec<(u64, Vec<Entry>)> = Vec::new();
        let mut events = 0usize;
        let mut exhausted = false;
        let mut read_step = self.config.read_step.min(64 * 1024);
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
            let step = read_step.min(budget - read).min(pending_start as usize);
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
            read_step = read_step.saturating_mul(2).min(self.config.read_step);
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
        usage: &mut [u64; 20],
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
        usage: &mut [u64; 20],
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
        if stamp.window_bytes() > self.config.source as u64 {
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
                .latest_source(&stamp.identity)
                .filter(|snapshot| snapshot.stamp == stamp);
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
                let registry_generation = str_field(context, "registry_generation")?;
                let record = LOAD_SLOT_BYTES
                    + 2 * <Sha256 as Digest>::output_size()
                    + path.as_os_str().len()
                    + registry_generation.len();
                let additional = state.loads.growth_for(&stamp.identity)
                    + record
                    + ActivityIndex::EMPTY_ALLOCATION_BYTES
                    + if cached.is_some() {
                        0
                    } else {
                        self.config.read_step.min(stamp.size as usize)
                    };
                self.admit_memory(&mut state, context, additional)?;
                let previous = state.source_spine(stamp);
                let previous_chunks = previous.as_ref().and_then(|previous| {
                    state
                        .generations
                        .get(&snapshot_key(previous))
                        .and_then(|generation| generation.source_chunks.clone())
                });
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
                    usage[match previous.as_ref() {
                        Some(previous) if previous.stamp.size < stamp.size => 5,
                        Some(_) => 7,
                        None => 4,
                    }] += 1;
                }
                let slot = Arc::new(LoadSlot {
                    id: self.token("load"),
                    path,
                    stamp,
                    registry_generation: registry_generation.to_owned(),
                    registry,
                    accounted: AtomicUsize::new(ActivityIndex::EMPTY_ALLOCATION_BYTES),
                    attached: AtomicBool::new(false),
                    deadline: AtomicU64::new(now + self.config.preparation),
                    work: Mutex::new(Load {
                        charge_work: Work::default(),
                        fresh_chunk_bytes: 0,
                        activity_from_previous: false,
                        codex_append_bytes: 0,
                        file,
                        offset: 0,
                        pending: Vec::new(),
                        pending_start: 0,
                        provider: None,
                        chunks: Vec::new(),
                        source_chunks: Vec::new(),
                        previous_chunks,
                        published_chunks: None,
                        reuse_next: None,
                        reuse_prepared: false,
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
                assert_eq!(
                    slot.record_bytes(),
                    record,
                    "load slot landed off its predicted record"
                );
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
            let pledge = Delivery::cursor_pledge(&waiter.claimant, &cursor);
            let additional =
                charged_bytes(&cursor, &waiter) + state.waiters.growth_for(&cursor) + pledge;
            if let Err(error) = self.admit_memory(&mut state, context, additional) {
                if created {
                    state.remove_load(&stamp.identity);
                }
                return Err(error);
            }
            state.insert_waiter(cursor.clone(), waiter.clone());
            state.waiters.pledge(&cursor, pledge);
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
        usage: &mut [u64; 20],
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
            let decode_bound = decode_source.saturating_mul(4);
            let reservation = read_bound.saturating_add(decode_bound);
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
                    decode_bound,
                    waiter.limits.max_events.saturating_sub(waiter.used_events),
                    &waiter.context,
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
            let generation = load.result.as_ref().map(|snapshot| {
                let mut record = GenerationRecord::new(snapshot, &slot.registry_generation);
                record.source_chunks = load.published_chunks.clone();
                record
            });
            let publishing = generation.is_some();
            let pending_charge = if publishing {
                0
            } else {
                Self::unpublished_load_charge(&load)
            };
            let pledge = generation
                .as_ref()
                .map(|_| {
                    let token = self.token("lease");
                    Lease::pledge(&token, &waiter.context, &waiter.classifier)
                        .map(|bytes| (token, bytes))
                })
                .transpose()?;
            let admitted = {
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
                match self.admit_memory(
                    &mut state,
                    &waiter.context,
                    additional.saturating_sub(slot.ledgered_bytes()),
                ) {
                    Ok(()) => {
                        if let Some((snapshot, record)) = generation {
                            state.register_generation(snapshot, record);
                            state.escape_chunks(&snapshot.chunks);
                        }
                        if let Some((_, bytes)) = &pledge {
                            state.transient_bytes += bytes;
                        }
                        state.set_load_charge(&slot, pending_charge);
                        lease = pledge.map(|(token, bytes)| {
                            (token, ProjectionReservation { store: self, bytes })
                        });
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = admitted {
                if !publishing {
                    Self::restart_unpublished_build(&slot, &mut load);
                    let remaining = Self::unpublished_load_charge(&load);
                    self.lock_state().set_load_charge(&slot, remaining);
                }
                return Err(error);
            }
        }
        if let Some(Err(error)) = stepped {
            if !matches!(
                error.status,
                Status::Cancelled | Status::Deadline | Status::Incomplete | Status::RetainedLimit
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
            let fresh = !state.holds_latest(&slot.stamp.identity, &snapshot);
            if fresh {
                let escaping = state.escape_growth(&snapshot.chunks);
                if let Err(error) = self.admit_memory(&mut state, &waiter.context, escaping) {
                    state.waiters.remove(token);
                    return Err(error);
                }
                state.escape_chunks(&snapshot.chunks);
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
            if fresh && !state.holds_latest(&slot.stamp.identity, &source) {
                let latest = state.latest_growth(&slot.stamp.identity, &source);
                if self
                    .admit_memory(&mut state, &waiter.context, latest)
                    .is_ok()
                {
                    usage[9] += 1;
                    if state.insert_latest(slot.stamp.identity, source).is_some() {
                        usage[10] += 1;
                    }
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

    #[cfg(test)]
    fn before_decode(&self, stage: &'static str) {
        let hook = self.decode_hook.lock().expect("decode hook").clone();
        if let Some(hook) = hook {
            hook(stage);
        }
    }

    #[cfg(not(test))]
    fn before_decode(&self, _stage: &'static str) {}

    fn after_location_park(&self) {
        #[cfg(test)]
        {
            let hook = self.locate_hook.lock().expect("locate hook").take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    #[cfg(test)]
    fn after_location_metadata(&self, stage: &'static str) {
        let hook = {
            let mut hook = self
                .location_metadata_hook
                .lock()
                .expect("location metadata hook");
            if hook
                .as_ref()
                .is_some_and(|(expected, _)| *expected == stage)
            {
                hook.take().map(|(_, hook)| hook)
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn after_source_resolved(&self, path: &Path) {
        let hook = self.resolve_hook.lock().expect("resolve hook").clone();
        if let Some(hook) = hook {
            hook(path);
        }
    }

    #[cfg(not(test))]
    fn after_source_resolved(&self, _path: &Path) {}

    fn after_facts_returned(&self) {
        #[cfg(test)]
        {
            let hook = self
                .returned_facts_hook
                .lock()
                .expect("returned facts hook")
                .take();
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
        usage: &mut [u64; 20],
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

    fn unpublished_load_charge(load: &Load) -> usize {
        load.charge_work.tick(1);
        let previous_activity = load.activity_from_previous.then(|| {
            load.previous
                .as_ref()
                .expect("inherited activity")
                .activity
                .as_ref()
        });
        load.pending.capacity()
            + load.codex_raw.as_ref().map_or(0, |raw| {
                if load
                    .previous
                    .as_ref()
                    .and_then(|previous| previous.codex_raw.as_ref())
                    .is_some_and(|previous| Arc::ptr_eq(previous, raw))
                {
                    0
                } else {
                    arc_bytes::<Vec<u8>>() + raw.capacity()
                }
            })
            + load.codex_append_bytes
            + load.origin_fence.capacity()
            + load.seal_fence.capacity()
            + load.prefix_fence.capacity()
            + load.fence.capacity()
            + load.session_id.as_ref().map_or(0, String::capacity)
            + load.chunks.capacity() * size_of::<Arc<EntryChunk>>()
            + load.source_chunks.capacity() * size_of::<SourceChunkSpan>()
            + load.fresh_chunk_bytes
            + load.activity.heap_bytes_since(previous_activity)
    }

    fn restart_unpublished_build(slot: &LoadSlot, load: &mut Load) {
        load.offset = 0;
        load.pending = Vec::new();
        load.pending_start = 0;
        load.provider = None;
        load.chunks = Vec::new();
        load.fresh_chunk_bytes = 0;
        load.source_chunks = Vec::new();
        load.published_chunks = None;
        load.reuse_next = None;
        load.reuse_prepared = false;
        load.codex_raw = None;
        load.codex_append = None;
        load.codex_append_bytes = 0;
        load.count = 0;
        load.activity = ActivityIndex::default();
        load.activity_from_previous = false;
        load.indexed = 0;
        load.decoded = false;
        load.session_id = None;
        load.origin_fence = Vec::new();
        load.origin_complete = false;
        load.seal_fence = Vec::new();
        load.sealed = false;
        load.prefix_fence = Vec::new();
        load.prefix_checked = false;
        load.window_scanned = 0;
        load.window_start = (slot.stamp.identity.window_base == 0).then_some(0);
        load.fence = Vec::new();
        load.committed = 0;
        load.provisional = false;
    }

    fn decoded_line_bytes(
        entries: &Vec<Entry>,
        parsed: &[Entry],
        arenas: &Vec<(usize, SourceArena)>,
        fresh: &[SourceArena],
        chunks: &Vec<Arc<EntryChunk>>,
        session_pending: bool,
    ) -> usize {
        if parsed.is_empty() {
            return 0;
        }
        let chunk = if entries.is_empty() {
            arc_bytes::<EntryChunk>() + arc_bytes::<ChunkRows>() + vec_growth(chunks, 1)
        } else {
            0
        };
        let session = if session_pending {
            parsed
                .iter()
                .find_map(Entry::meta)
                .map_or(0, |meta| meta.session_id.len())
        } else {
            0
        };
        chunk
            + session
            + vec_growth(entries, parsed.len())
            + vec_growth(arenas, fresh.len())
            + parsed
                .iter()
                .map(|entry| {
                    let charge = entry_charge(entry);
                    charge.owned_capacity_bytes
                        + charge.opaque_dom_accounted_bytes
                        + size_of::<MemoryCharge>()
                })
                .sum::<usize>()
            + fresh
                .iter()
                .map(|arena| arena_bytes(&arena.root, arena.source_len))
                .sum::<usize>()
    }

    fn decoded_line_bound(
        entries: &Vec<Entry>,
        arenas: &Vec<(usize, SourceArena)>,
        chunks: &Vec<Arc<EntryChunk>>,
        session_pending: bool,
        dom: &Value,
        source_len: usize,
        fence_growth: usize,
    ) -> Option<usize> {
        let retained = crate::parse::retained_arena_bound(dom);
        let chunk = if entries.is_empty() {
            arc_bytes::<EntryChunk>() + arc_bytes::<ChunkRows>() + vec_growth(chunks, 1)
        } else {
            0
        };
        let session = if session_pending {
            crate::value::field_str(dom, "sessionId").map_or(0, str::len)
        } else {
            0
        };
        [
            chunk,
            session,
            vec_growth(entries, 1),
            size_of::<MemoryCharge>(),
            fence_growth,
            crate::parse::retained_entry_bound(dom)?,
            arena_bytes(dom, source_len),
            vec_growth(arenas, retained),
            crate::parse::pushed_capacity(retained).checked_mul(size_of::<SourceArena>())?,
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }

    fn admit_decode(
        &self,
        slot: &LoadSlot,
        context: &Value,
        needed: usize,
        admitted: &mut usize,
        deferrable: bool,
    ) -> Result<bool, SnapshotError> {
        if needed <= *admitted {
            return Ok(true);
        }
        match self.extend_load_reservation(slot, context, needed - *admitted) {
            Ok(()) => {
                *admitted = needed;
                Ok(true)
            }
            Err(error) if deferrable && error.status == Status::RetainedLimit => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn decode_line(
        &self,
        slot: &LoadSlot,
        context: &Value,
        line: &[u8],
        entries: &Vec<Entry>,
        arenas: &Vec<(usize, SourceArena)>,
        chunks: &Vec<Arc<EntryChunk>>,
        session_pending: bool,
        fence_growth: usize,
        decoded: usize,
        admitted: &mut usize,
        deferrable: bool,
    ) -> Result<LineDecode, SnapshotError> {
        let overflow = || SnapshotError::new(Status::RetainedLimit, "line bound overflows");
        let dom_bound = dom_parse_bound(line.len()).ok_or_else(overflow)?;
        if !self.admit_decode(slot, context, decoded + dom_bound, admitted, deferrable)? {
            return Ok(LineDecode::Deferred);
        }
        self.before_decode("dom");
        let Some(dom) = crate::parse::parse_line_dom(line) else {
            return Ok(LineDecode::Decoded {
                entry: None,
                arenas: Vec::new(),
                charge: fence_growth,
            });
        };
        let typed_bound = Self::decoded_line_bound(
            entries,
            arenas,
            chunks,
            session_pending,
            &dom,
            line.len(),
            fence_growth,
        )
        .ok_or_else(overflow)?;
        let needed = decoded
            .checked_add(dom_bound)
            .and_then(|needed| needed.checked_add(typed_bound))
            .ok_or_else(overflow)?;
        if !self.admit_decode(slot, context, needed, admitted, deferrable)? {
            return Ok(LineDecode::Deferred);
        }
        self.before_decode("entry");
        let root = dom.clone();
        let mut retained = crate::parse::Retained::default();
        let entry = crate::parse::entry_from_dom(dom, &mut retained, &|_| true)
            .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
        let fresh = if entry.is_some() {
            retained.arenas(root, line.len())
        } else {
            Vec::new()
        };
        let charge = Self::decoded_line_bytes(
            entries,
            entry.as_slice(),
            arenas,
            &fresh,
            chunks,
            session_pending,
        ) + fence_growth;
        assert!(
            typed_bound >= charge,
            "a decoded line outgrew its admitted bound"
        );
        Ok(LineDecode::Decoded {
            entry,
            arenas: fresh,
            charge,
        })
    }

    fn fence_growth(fence: &Vec<u8>, fresh: &[u8]) -> usize {
        let after = if fresh.len() >= 64 {
            64
        } else {
            vec_capacity_for(fence, fresh.len())
        };
        after.saturating_sub(fence.capacity())
    }

    fn unindexed(
        chunks: &[Arc<EntryChunk>],
        indexed: usize,
        stop: usize,
    ) -> impl Iterator<Item = (&Entry, &MemoryCharge)> + '_ {
        let first = chunks
            .partition_point(|chunk| chunk.start <= indexed)
            .saturating_sub(1);
        chunks[first..]
            .iter()
            .flat_map(move |chunk| {
                chunk
                    .entries
                    .iter()
                    .zip(chunk.entry_charges.iter())
                    .skip(indexed.saturating_sub(chunk.start))
            })
            .take(stop - indexed)
    }

    fn extend_load_reservation(
        &self,
        slot: &LoadSlot,
        context: &Value,
        bytes: usize,
    ) -> Result<(), SnapshotError> {
        let mut state = self.lock_state();
        self.admit_memory(&mut state, context, bytes)?;
        state.add_load_charge(slot, bytes);
        Ok(())
    }

    fn step(
        &self,
        slot: &LoadSlot,
        load: &mut Load,
        read_bound: usize,
        events_bound: usize,
        decode_bound: usize,
        lowering_events: usize,
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
        usage: &mut [u64; 20],
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
                    if previous.stamp.identity.window_base != slot.stamp.identity.window_base {
                        load.file
                            .seek(SeekFrom::Start(load.offset))
                            .map_err(io_error)?;
                        load.prefix_checked = true;
                        return Ok(());
                    }
                    load.reuse_prepared = true;
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
                        load.codex_append_bytes = 0;
                        if load.previous_index_compatible {
                            load.activity = previous.activity.as_ref().clone();
                            load.activity_from_previous = true;
                            load.indexed = previous.event_count;
                        }
                    } else {
                        let keep = previous
                            .chunks
                            .len()
                            .saturating_sub(usize::from(previous.provisional_tail));
                        load.chunks = previous.chunks[..keep].to_vec();
                        if let Some(spans) = &load.previous_chunks {
                            self.extend_load_reservation(
                                slot,
                                context,
                                keep * size_of::<SourceChunkSpan>(),
                            )?;
                            load.source_chunks = spans[..keep].to_vec();
                        }
                        load.count = load.chunks.iter().map(|chunk| chunk.entries.len()).sum();
                        load.committed = previous.committed_bytes;
                        load.offset = previous.committed_bytes;
                        load.pending_start = previous.committed_bytes;
                        load.window_start = Some(previous.window_start);
                        load.provider = Some(Provider::Claude);
                        load.session_id = Some(previous.session_id.clone());
                        if !previous.provisional_tail && load.previous_index_compatible {
                            load.activity = previous.activity.as_ref().clone();
                            load.activity_from_previous = true;
                            load.indexed = previous.event_count;
                        }
                        load.fence = fence;
                    }
                    load.prefix_checked = true;
                } else {
                    load.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
                    self.lock_state()
                        .forget_source_spine(&previous.stamp.identity, previous);
                    load.previous = None;
                    load.previous_chunks = None;
                    usage[4] += 1;
                }
                return Ok(());
            }
        }
        if load.window_start.is_none() {
            let base = slot.stamp.identity.window_base;
            let floor = load
                .previous_chunks
                .as_ref()
                .and_then(|spans| {
                    let at = spans
                        .partition_point(|span| span.start <= base)
                        .checked_sub(1)?;
                    let span = spans[at];
                    (base <= span.end).then_some(if base == span.end {
                        span.end
                    } else {
                        span.start
                    })
                })
                .unwrap_or(0);
            let unscanned = base - load.window_scanned;
            let located = if unscanned == floor {
                Some(floor)
            } else {
                if load.window_scanned > self.config.entry as u64 {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                let count = (read_bound as u64)
                    .min((64 * 1024u64).saturating_add(load.window_scanned))
                    .min(unscanned - floor);
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
            let (bytes, calls, results) = Self::unindexed(&load.chunks, load.indexed, stop).fold(
                (0usize, 0usize, 0usize),
                |(bytes, calls, results), (entry, charge)| {
                    (
                        bytes + charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
                        calls + entry.tool_uses().count(),
                        results + entry.tool_results().count(),
                    )
                },
            );
            let extension = bytes
                .saturating_mul(2)
                .saturating_add((stop - load.indexed) * size_of::<&Entry>())
                .saturating_add(load.activity.append_container_reservation_bytes(
                    stop - load.indexed,
                    calls,
                    results,
                ));
            self.extend_load_reservation(slot, context, extension)?;
            let entries: Vec<&Entry> = Self::unindexed(&load.chunks, load.indexed, stop)
                .map(|(entry, _)| entry)
                .collect();
            load.activity = std::mem::take(&mut load.activity).append_tail(&entries, None);
            load.indexed = stop;
            usage[6] += 1;
            return Ok(());
        }
        if self.reuse_source_chunk(slot, load, context)? {
            return Ok(());
        }
        if !load.decoded {
            if load.offset < slot.stamp.size
                && (load.pending.is_empty()
                    || load.provider == Some(Provider::Codex)
                    || !load.pending.contains(&b'\n'))
            {
                let stop = load.reuse_next.map_or(slot.stamp.size, |index| {
                    load.previous_chunks.as_ref().unwrap()[index].start
                });
                let count = (stop - load.offset).min(read_bound as u64) as usize;
                let growth = vec_growth(&load.pending, count);
                if growth > read_bound {
                    self.extend_load_reservation(slot, context, growth - read_bound)?;
                }
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
                let mut arenas = Vec::new();
                let mut consumed = 0;
                let mut lines = 0;
                let mut decoded = 0usize;
                let mut admitted = decode_bound;
                let mut decode_bound = decode_bound;
                let mut session_pending = load.session_id.is_none();
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
                    let fresh = &load.pending[consumed..=end];
                    let LineDecode::Decoded {
                        entry,
                        arenas: retained,
                        charge,
                    } = self.decode_line(
                        slot,
                        context,
                        &load.pending[consumed..end],
                        &entries,
                        &arenas,
                        &load.chunks,
                        session_pending,
                        Self::fence_growth(&load.fence, fresh),
                        decoded,
                        &mut admitted,
                        lines > 0,
                    )?
                    else {
                        break;
                    };
                    if decoded + charge > decode_bound {
                        if lines > 0 {
                            break;
                        }
                        decode_bound = decoded + charge;
                    }
                    decoded += charge;
                    if session_pending && entry.as_ref().is_some_and(|entry| entry.meta().is_some())
                    {
                        session_pending = false;
                    }
                    arenas.extend(retained.into_iter().map(|arena| (entries.len(), arena)));
                    entries.extend(entry);
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
                    let source_growth = vec_growth(&load.source_chunks, 1);
                    if source_growth > 0 {
                        self.extend_load_reservation(slot, context, source_growth)?;
                    }
                    load.source_chunks.push(SourceChunkSpan {
                        start: load.pending_start,
                        end: load.pending_start + consumed as u64,
                    });
                    let chunk = Arc::new(EntryChunk::retaining(load.count, entries, arenas));
                    load.fresh_chunk_bytes +=
                        chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes;
                    load.chunks.push(chunk);
                    load.count += count;
                }
                if consumed > 0 {
                    if let Some(span) = load.source_chunks.last_mut() {
                        span.end = load.pending_start + consumed as u64;
                    }
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
                    let LineDecode::Decoded {
                        entry,
                        arenas: retained,
                        charge,
                    } = self.decode_line(
                        slot,
                        context,
                        &load.pending,
                        &Vec::new(),
                        &Vec::new(),
                        &load.chunks,
                        load.session_id.is_none(),
                        0,
                        decoded,
                        &mut admitted,
                        lines > 0,
                    )?
                    else {
                        return Ok(());
                    };
                    if decoded + charge > decode_bound && lines > 0 {
                        return Ok(());
                    }
                    let mut tail = Vec::new();
                    tail.extend(entry);
                    let mut arenas = Vec::new();
                    arenas.extend(retained.into_iter().map(|arena| (0, arena)));
                    usage[2] += load.pending.len() as u64;
                    usage[3] += 1;
                    let count = tail.len();
                    if load.session_id.is_none() {
                        load.session_id = tail
                            .iter()
                            .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
                    }
                    let source_growth = vec_growth(&load.source_chunks, 1);
                    if source_growth > 0 {
                        self.extend_load_reservation(slot, context, source_growth)?;
                    }
                    load.source_chunks.push(SourceChunkSpan {
                        start: load.pending_start,
                        end: slot.stamp.size,
                    });
                    let chunk = Arc::new(EntryChunk::retaining(load.count, tail, arenas));
                    load.fresh_chunk_bytes +=
                        chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes;
                    load.chunks.push(chunk);
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
            SourceStamp::of(&std::fs::metadata(&slot.path).map_err(io_error)?),
            slot.stamp,
        ) {
            return Err(SnapshotError::new(
                Status::Changed,
                "source changed before publication",
            ));
        }
        let session_id = load.session_id.clone().unwrap_or_else(|| {
            slot.path
                .file_stem()
                .expect("source filename")
                .to_string_lossy()
                .into_owned()
        });
        if load.provider == Some(Provider::Claude) && load.source_chunks.len() == load.chunks.len()
        {
            self.extend_load_reservation(slot, context, arc_bytes::<Vec<SourceChunkSpan>>())?;
            load.published_chunks = Some(Arc::new(std::mem::take(&mut load.source_chunks)));
        }
        load.result = Some(Arc::new(TranscriptSnapshot {
            ledger: LedgerHook::default(),
            id: self.token("snapshot"),
            canonical_path: slot.path.clone(),
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
        load.chunks = Vec::new();
        load.fresh_chunk_bytes = 0;
        load.fence = Vec::new();
        load.origin_fence = Vec::new();
        load.seal_fence = Vec::new();
        load.session_id = None;
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
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut bounds = limits(request)?;
        cancel.check(bounds.deadline_unix_ms)?;
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        let root_handle = view
            .get("handle")
            .ok_or_else(|| invalid("missing root handle"))?;
        let scope = self.renew_scope_for_work(root_handle, context, bounds.deadline_unix_ms)?;
        bounds.deadline_unix_ms = bounds.deadline_unix_ms.min(scope.expires);
        if !classifier_eq(&view["classifier"], &scope.classifier)? {
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
        let attachments = view
            .get("attachments")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing attachments"))?;
        let attachment_bytes = attachments
            .iter()
            .filter(|_| !direct)
            .map(|attachment| {
                attachment
                    .as_str()
                    .map(str::len)
                    .ok_or_else(|| invalid("invalid attachment"))
            })
            .sum::<Result<usize, _>>()?;
        let task_count = if direct { 0 } else { attachments.len() } + usize::from(selected);
        let claimant = str_field(context, "claimant")?;
        let lease_id = str_field(root_handle, "lease_id")?;
        let root_path = scope.snapshot.canonical_path.as_os_str().len();
        let description_bytes = value_bytes(&self.description(
            &scope.snapshot,
            &scope.classifier,
            lease_id,
            scope.expires,
        ));
        let mut reservation = self.reserve_projection(
            context,
            size_of::<GraphCursor>()
                + claimant.len()
                + value_bytes(context)
                + value_bytes(request)
                + value_bytes(root_handle)
                + size_of::<GraphNode>()
                + root_path
                + description_bytes
                + set_growth(&HashSet::<SourceIdentity>::new(), 1)
                + task_count * size_of::<GraphTask>()
                + attachment_bytes
                + if selected { root_path } else { 0 },
        )?;
        #[cfg(test)]
        self.graph_records.fetch_add(1, Ordering::Relaxed);
        let LeasedScope {
            snapshot: root,
            classifier,
            expires,
            ..
        } = scope;
        let description = self.description(&root, &classifier, lease_id, expires);
        drop(classifier);
        let seen_capacity = set_capacity_for(&HashSet::<SourceIdentity>::new(), 1);
        let mut tasks = Vec::with_capacity(task_count);
        for attachment in attachments.iter().rev().filter(|_| !direct) {
            tasks.push(GraphTask::Visit {
                path: PathBuf::from(attachment.as_str().expect("validated attachment")),
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
            claimant: claimant.to_owned(),
            context: context.clone(),
            request: request.clone(),
            root_handle: root_handle.clone(),
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
        };
        assert_eq!(
            (graph.seen.capacity(), graph.tasks.capacity()),
            (seen_capacity, task_count)
        );
        self.graph_step(&self.token("graph"), graph, &mut reservation, cancel, usage)
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
        Self::prune(state);
    }

    fn graph_step(
        &self,
        token: &str,
        mut graph: GraphCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let (data, complete) = match self.graph_work(&mut graph, reservation, cancel, usage) {
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
        let token = token.to_owned();
        let pledge = Delivery::cursor_pledge(&graph.claimant, &token);
        let additional =
            state.admission(&token, &graph, []) + state.graphs.growth_for(&token) + pledge;
        let covered = additional.min(reservation.bytes);
        if let Err(error) = self.admit_memory(&mut state, &graph.context, additional - covered) {
            Self::rollback_graph_page(&mut state, &mut graph);
            return Err(error);
        }
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        graph.published_members.clear();
        state.graphs.reserve_for(&token);
        state.graphs.insert(token.clone(), graph);
        state.graphs.pledge(&token, pledge);
        Ok((data, Some(token), Some("graph work incomplete".to_owned())))
    }

    fn graph_add_source(
        &self,
        graph: &mut GraphCursor,
        path: PathBuf,
        depth: usize,
        spawned_by: Option<String>,
        data: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(), SnapshotError> {
        let acquired = data
            .get("description")
            .ok_or_else(|| invalid("acquire returned no source description"))?;
        let direct = graph.request["query"]["kind"].as_str() == Some("direct_sidechains");
        let pinned = self
            .extend_projection_reservation(
                reservation,
                &graph.context,
                value_bytes(acquired)
                    + set_growth(&graph.seen, 1)
                    + vec_growth(&graph.nodes, 1)
                    + path.as_os_str().len()
                    + if direct {
                        0
                    } else {
                        vec_growth(&graph.tasks, 1)
                    },
            )
            .and_then(|()| {
                #[cfg(test)]
                self.graph_sources.fetch_add(1, Ordering::Relaxed);
                self.pin_scope_for_work(
                    &acquired["handle"],
                    &graph.context,
                    graph.remaining.deadline_unix_ms,
                )
            });
        if pinned.is_err() {
            self.lock_state()
                .leases
                .remove(str_field(&acquired["handle"], "lease_id")?);
        }
        let (snapshot, description) = pinned?;
        let identity = snapshot.stamp.identity.file();
        if !graph.seen.insert(identity) && !direct {
            self.lock_state()
                .leases
                .remove(str_field(&description["handle"], "lease_id")?);
            return Ok(());
        }
        let predicted = (
            vec_capacity_for(&graph.nodes, 1),
            if direct {
                graph.tasks.capacity()
            } else {
                vec_capacity_for(&graph.tasks, 1)
            },
        );
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
        if !direct {
            graph.tasks.push(GraphTask::List {
                parent: path,
                depth: depth + 1,
            });
        }
        assert_eq!((graph.nodes.capacity(), graph.tasks.capacity()), predicted);
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
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        work: &mut usize,
        work_stop: usize,
    ) -> Result<bool, SnapshotError> {
        let canonical = std::fs::canonicalize(&pending.path).map_err(io_error);
        let token = pending.token.clone();
        graph.pending = Some(pending);
        self.authority(&graph.context, Some(&canonical?))?;
        loop {
            if *work >= work_stop {
                return Ok(false);
            }
            *work += 1;
            let waiter = self
                .rebind_waiter(&mut self.lock_state(), &token, &graph.context)?
                .ok_or_else(|| {
                    SnapshotError::new(Status::StaleCursor, "graph source reservation expired")
                })?;
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
                reservation,
            )?;
            return Ok(true);
        }
    }

    fn graph_work(
        &self,
        graph: &mut GraphCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<GraphYield, SnapshotError> {
        cancel.check(graph.remaining.deadline_unix_ms)?;
        self.renew_scope_for_work(
            &graph.root_handle,
            &graph.context,
            graph.remaining.deadline_unix_ms,
        )?;
        self.renew_graph_members(graph)?;
        let kind = str_field(&graph.request["query"], "kind")?;
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
        self.extend_projection_reservation(reservation, &graph.context, FILESYSTEM_PATH_BYTES)?;
        let work_stop = self.config.event_step.min(self.config.page_items);
        let mut examined = 0usize;
        if let Some(pending) = graph.pending.take() {
            if !self.advance_graph_source(
                graph,
                pending,
                reservation,
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
                    let path = entry.map_err(io_error)?.path();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                        && !path
                            .file_name()
                            .expect("directory entry name")
                            .as_encoded_bytes()
                            .starts_with(b"._")
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
                        self.extend_projection_reservation(
                            reservation,
                            &graph.context,
                            path.capacity() + vec_growth(&listing.children, 1),
                        )?;
                        let predicted = vec_capacity_for(&listing.children, 1);
                        listing.children.push(path);
                        assert_eq!(listing.children.capacity(), predicted);
                    }
                }
                listing.children.sort_unstable();
                self.extend_projection_reservation(
                    reservation,
                    &graph.context,
                    vec_growth(&graph.tasks, listing.children.len())
                        + listing
                            .children
                            .iter()
                            .map(|path| graph_spawned_by(path).len())
                            .sum::<usize>(),
                )?;
                let predicted = vec_capacity_for(&graph.tasks, listing.children.len());
                graph.tasks.reserve(listing.children.len());
                for path in listing.children.into_iter().rev() {
                    let spawned_by = graph_spawned_by(&path).into_owned();
                    graph.tasks.push(GraphTask::Visit {
                        path,
                        depth: listing.depth,
                        spawned_by: Some(spawned_by),
                    });
                }
                assert_eq!(graph.tasks.capacity(), predicted);
                continue;
            }
            let Some(task) = graph.tasks.pop() else {
                graph.prepared = true;
                break;
            };
            match task {
                GraphTask::List { parent, depth } => {
                    let base = parent
                        .parent()
                        .ok_or_else(|| invalid("source has no parent"))?;
                    let stem = parent
                        .file_stem()
                        .ok_or_else(|| invalid("source has no stem"))?;
                    self.extend_projection_reservation(
                        reservation,
                        &graph.context,
                        2 * sidechain_directory_capacity(base, stem) + READ_DIR_HANDLE_BYTES,
                    )?;
                    let directory = sidechain_directory(base, stem);
                    match std::fs::canonicalize(&directory) {
                        Ok(canonical) => {
                            self.authority(&graph.context, Some(&canonical))?;
                            graph.listing = Some(GraphListing {
                                entries: OpenDirectory::open(&directory)?,
                                children: Vec::new(),
                                depth,
                            });
                            #[cfg(test)]
                            self.directory_opens.fetch_add(1, Ordering::Relaxed);
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
                    let acquire = {
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
                        let canonical = canonical.to_string_lossy();
                        json!({"schema":SCHEMA,"id":"graph-source","operation":"acquire","path":canonical.as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":bounds.deadline_unix_ms,
                            "limits":bounds.to_json()})
                    };
                    self.extend_projection_reservation(
                        reservation,
                        &graph.context,
                        2 * <Sha256 as Digest>::output_size(),
                    )?;
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
                            reservation,
                            cancel,
                            usage,
                            &mut examined,
                            work_stop,
                        )? {
                            return Ok(GraphYield::Pending(Value::new_null()));
                        }
                    } else {
                        self.graph_add_source(
                            graph,
                            path,
                            depth,
                            spawned_by,
                            &outcome.0,
                            reservation,
                        )?;
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
            let (index, record, fresh) = if let Some((index, record)) =
                graph.pending_records.pop_front()
            {
                (index, record, false)
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
                        true,
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
                    let mut member_records = member_records.into_iter();
                    let first = member_records.next().expect("predicate member record");
                    self.extend_projection_reservation(
                        reservation,
                        &graph.context,
                        deque_growth(&graph.pending_records, member_records.len())
                            + member_records
                                .as_slice()
                                .iter()
                                .map(String::capacity)
                                .sum::<usize>(),
                    )?;
                    let predicted =
                        deque_capacity_for(&graph.pending_records, member_records.len());
                    graph.pending_records.reserve(member_records.len());
                    graph
                        .pending_records
                        .extend(member_records.map(|record| (index, record)));
                    assert_eq!(graph.pending_records.capacity(), predicted);
                    (index, first, true)
                }
            };
            let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)?;
            if output_bytes.saturating_add(record_bytes)
                > graph.remaining.max_output_bytes.min(MAX_DATA_BYTES)
            {
                if fresh {
                    self.extend_projection_reservation(
                        reservation,
                        &graph.context,
                        deque_growth(&graph.pending_records, 1) + record.capacity(),
                    )?;
                }
                let predicted = deque_capacity_for(&graph.pending_records, 1);
                graph.pending_records.push_front((index, record));
                assert_eq!(graph.pending_records.capacity(), predicted);
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
                self.extend_projection_reservation(
                    reservation,
                    &graph.context,
                    vec_growth(&graph.published_members, 1),
                )?;
                graph.nodes[index].transferred = true;
                let predicted = vec_capacity_for(&graph.published_members, 1);
                graph.published_members.push(index);
                assert_eq!(graph.published_members.capacity(), predicted);
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
        request: Cow<'_, Value>,
        context: &Value,
        cancel: &Cancellation,
        mut bound: WorkLimits,
        next: usize,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(bound.deadline_unix_ms)?;
        bound.max_output_bytes = bound.max_output_bytes.min(self.config.output);
        if str_field(&request, "operation")? == "hydrate" {
            return self.hydrate(request, context, cancel, bound, next, reservation);
        }
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        let handle = view
            .get("handle")
            .ok_or_else(|| invalid("missing view handle"))?;
        let scope = self.renew_scope_for_work(handle, context, bound.deadline_unix_ms)?;
        let snapshot = scope.snapshot;
        bound.deadline_unix_ms = bound.deadline_unix_ms.min(scope.expires);
        if !classifier_eq(&view["classifier"], &scope.classifier)? {
            return Err(invalid("view classifier differs from leased generation"));
        }
        let mut page = bound;
        page.max_output_bytes = page.max_output_bytes.min(MAX_DATA_BYTES);
        page.max_items = page.max_items.min(self.config.page_items);
        let projection = if str_field(&request, "operation")? == "classify_view" {
            self.classify_view_projection(snapshot, &request, context, cancel, page, next)?
        } else if str_field(&request, "operation")? == "query"
            && request["query"]["kind"].as_str() == Some("meta_messages")
        {
            self.meta_message_projection(
                snapshot,
                &request,
                context,
                cancel,
                page,
                next,
                reservation,
            )?
        } else if str_field(&request, "operation")? == "query"
            && matches!(
                request["query"]["kind"].as_str(),
                Some("interaction_keys" | "interaction_winners" | "interaction_records")
            )
        {
            self.interaction_projection(
                snapshot,
                &request,
                context,
                cancel,
                page,
                next,
                reservation,
            )?
        } else if str_field(&request, "operation")? == "mine" {
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
            callback(snapshot, &request, &page, cancel, next)?
        } else {
            let mut local = Value::clone(&request);
            local["view"].insert("attachments", json!([]));
            crate::snapshot_projection::project(&snapshot, &local, &page, cancel, next)?
        };
        self.projection_result(request, context, bound, projection, reservation)
    }

    fn projection_result(
        &self,
        request: Cow<'_, Value>,
        context: &Value,
        mut bound: WorkLimits,
        projection: Projection,
        reservation: &mut ProjectionReservation<'_>,
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
                let token = self.token("projection");
                let cursor = ProjectionCursor {
                    claimant: str_field(context, "claimant")?.to_owned(),
                    registry_generation: str_field(context, "registry_generation")?.to_owned(),
                    admission: str_field(context, "admission")?.to_owned(),
                    request: request.into_owned(),
                    limits: bound,
                    next,
                    expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
                };
                let pledge = Delivery::cursor_pledge(&cursor.claimant, &token);
                let additional = state.admission(&token, &cursor, [])
                    + state.projections.growth_for(&token)
                    + pledge;
                let covered = additional.min(reservation.bytes);
                self.admit_memory(&mut state, context, additional - covered)?;
                state.transient_bytes -= covered;
                reservation.bytes -= covered;
                state.projections.reserve_for(&token);
                state.projections.insert(token.clone(), cursor);
                state.projections.pledge(&token, pledge);
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
        request: Cow<'_, Value>,
        context: &Value,
        cancel: &Cancellation,
        mut bound: WorkLimits,
        next: usize,
        reservation: &mut ProjectionReservation<'_>,
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
            let scope = self.renew_scope_for_work(
                handle
                    .get("handle")
                    .ok_or_else(|| invalid("missing handle"))?,
                context,
                bound.deadline_unix_ms,
            )?;
            let snapshot = scope.snapshot;
            bound.deadline_unix_ms = bound.deadline_unix_ms.min(scope.expires);
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
                let mut single = Value::clone(&request);
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
        let projection = Projection {
            data: json!({"kind": "hydrated", "windows": output}),
            complete: at == windows.len(),
            next: (at < windows.len() && at > next).then_some(at),
            reason: (at < windows.len()).then(|| "hydration work incomplete".to_owned()),
            read_bytes,
            events,
            items: count,
        };
        self.projection_result(request, context, bound, projection, reservation)
    }

    fn discover(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
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
        let claimant = str_field(context, "claimant")?;
        let mut reservation = self.reserve_projection(
            context,
            claimant.len()
                + value_bytes(request)
                + value_bytes(context)
                + roots.len() * size_of::<PathBuf>()
                + LOCATE_PATH_SLOTS,
        )?;
        #[cfg(test)]
        self.discovery_records.fetch_add(1, Ordering::Relaxed);
        let mut paths = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            let path = std::fs::canonicalize(root.as_str().ok_or_else(|| invalid("invalid root"))?)
                .map_err(io_error)?;
            self.authority(context, Some(&path))?;
            self.extend_projection_reservation(&mut reservation, context, path.capacity())?;
            paths.push(path);
        }
        assert_eq!(paths.capacity(), roots.len());
        let previous = if let Some(token) = request.get("checkpoint").and_then(Value::as_str) {
            let mut state = self.lock_state();
            Self::prune(&mut state);
            let checkpoint = state.checkpoints.get(token).ok_or_else(|| {
                SnapshotError::new(Status::StaleCursor, "discovery checkpoint expired")
            })?;
            if checkpoint.claimant != claimant
                || sonic_rs::to_vec(&checkpoint.roots).ok()
                    != sonic_rs::to_vec(&request["roots"]).ok()
            {
                return Err(SnapshotError::new(
                    Status::StaleCursor,
                    "checkpoint scope differs",
                ));
            }
            let inventory = inventory_bytes(&checkpoint.inventory);
            self.extend_projection_reservation_in(
                &mut state,
                &mut reservation,
                context,
                inventory,
            )?;
            state
                .checkpoints
                .get(token)
                .expect("checked checkpoint")
                .inventory
                .clone()
        } else {
            HashMap::new()
        };
        let token = self.token("discovery");
        let cursor = DiscoveryCursor {
            claimant: claimant.to_owned(),
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
            inventory_payload: 0,
            previous,
            removed: Vec::new(),
            walking: true,
            expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
        };
        self.scan(&token, cursor, &mut reservation, cancel, usage)
    }

    fn scan(
        &self,
        token: &str,
        mut scan: DiscoveryCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
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
                    let removed = scan
                        .previous
                        .keys()
                        .filter(|path| !scan.inventory.contains_key(*path))
                        .count();
                    self.extend_projection_reservation(
                        reservation,
                        &scan.context,
                        removed * size_of::<Value>(),
                    )?;
                    scan.removed = Vec::with_capacity(removed);
                    scan.removed.extend(
                        scan.previous
                            .drain()
                            .filter(|(path, _)| !scan.inventory.contains_key(path))
                            .map(|(_, mut value)| {
                                value.insert("state", json!("removed"));
                                value
                            }),
                    );
                    assert_eq!(scan.removed.capacity(), removed);
                    break;
                };
                self.authority(&scan.context, Some(&path))?;
                let metadata = std::fs::metadata(&path).map_err(io_error)?;
                if metadata.is_dir() {
                    let identity = SourceStamp::of(&metadata).identity;
                    if scan.seen_directories.contains(&identity) {
                        continue;
                    }
                    self.extend_projection_reservation(
                        reservation,
                        &scan.context,
                        set_growth(&scan.seen_directories, 1)
                            + vec_growth(&scan.directories, 1)
                            + READ_DIR_HANDLE_BYTES
                            + path.as_os_str().len(),
                    )?;
                    let predicted = (
                        set_capacity_for(&scan.seen_directories, 1),
                        vec_capacity_for(&scan.directories, 1),
                    );
                    scan.seen_directories.insert(identity);
                    scan.directories.push(OpenDirectory::open(&path)?);
                    #[cfg(test)]
                    self.directory_opens.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(
                        (
                            scan.seen_directories.capacity(),
                            scan.directories.capacity()
                        ),
                        predicted
                    );
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
                let symlink = match entry.symlink_hint(self) {
                    Some(symlink) => symlink,
                    None => {
                        entry
                            .directory
                            .object_metadata(&entry.name, self)
                            .map_err(io_error)?
                            .symlink
                    }
                };
                let canonical = if symlink {
                    std::fs::canonicalize(&path).map_err(io_error)?
                } else {
                    path.clone()
                };
                self.authority(&scan.context, Some(&canonical))?;
                let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                if metadata.is_dir() {
                    if scan
                        .request
                        .get("follow_directory_symlinks")
                        .and_then(Value::as_bool)
                        == Some(false)
                        && symlink
                    {
                        continue;
                    }
                    self.extend_projection_reservation(
                        reservation,
                        &scan.context,
                        vec_growth(&scan.roots, 1) + canonical.capacity(),
                    )?;
                    let predicted = vec_capacity_for(&scan.roots, 1);
                    scan.roots.push(canonical);
                    assert_eq!(scan.roots.capacity(), predicted);
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
            let fresh = !scan.seen.contains(&stamp.identity);
            if !fresh
                && scan
                    .request
                    .get("preserve_aliases")
                    .and_then(Value::as_bool)
                    != Some(true)
            {
                continue;
            }
            if fresh {
                self.extend_projection_reservation(
                    reservation,
                    &scan.context,
                    set_growth(&scan.seen, 1),
                )?;
                let predicted = set_capacity_for(&scan.seen, 1);
                scan.seen.insert(stamp.identity);
                assert_eq!(scan.seen.capacity(), predicted);
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
                output.push(value.clone());
            }
            let (table, growth) = if scan.inventory.contains_key(&path) {
                (scan.inventory.capacity(), 0)
            } else {
                (
                    map_capacity_for(&scan.inventory, 1),
                    map_growth(&scan.inventory, 1) + path.capacity(),
                )
            };
            self.extend_projection_reservation(
                reservation,
                &scan.context,
                growth + value_bytes(&value),
            )?;
            scan.inventory_payload += value_bytes(&value);
            match scan.inventory.get_mut(&path) {
                Some(slot) => {
                    scan.inventory_payload -= value_bytes(slot);
                    *slot = value;
                }
                None => {
                    scan.inventory_payload += path.capacity();
                    scan.inventory.insert(path, value);
                }
            }
            assert_eq!(scan.inventory.capacity(), table);
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
        if complete {
            self.extend_projection_reservation_in(
                &mut state,
                reservation,
                &scan.context,
                value_bytes(&scan.request["roots"]),
            )?;
            let checkpoint = self.token("checkpoint");
            let record = Checkpoint {
                claimant: scan.claimant,
                roots: scan.request["roots"].clone(),
                inventory: scan.inventory,
                expires: now_ms() + self.config.ttl,
            };
            let additional = state.admission(&checkpoint, &record, [])
                + state.checkpoints.growth_for(&checkpoint);
            let covered = additional.min(reservation.bytes);
            self.admit_memory(&mut state, &scan.context, additional - covered)?;
            state.transient_bytes -= covered;
            reservation.bytes -= covered;
            state.checkpoints.reserve_for(&checkpoint);
            state.checkpoints.insert(checkpoint.clone(), record);
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
            let token = token.to_owned();
            let pledge = Delivery::cursor_pledge(&scan.claimant, &token);
            let additional =
                state.admission(&token, &scan, []) + state.discoveries.growth_for(&token) + pledge;
            let covered = additional.min(reservation.bytes);
            self.admit_memory(&mut state, &scan.context, additional - covered)?;
            state.transient_bytes -= covered;
            reservation.bytes -= covered;
            state.discoveries.reserve_for(&token);
            state.discoveries.insert(token.clone(), scan);
            state.discoveries.pledge(&token, pledge);
            Ok((
                json!({"kind":"discovered","entries":output,"checkpoint":null}),
                Some(token),
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

    fn queue_unfound_locations(
        &self,
        cursor: &mut LocateCursor,
        reservation: &mut ProjectionReservation<'_>,
        status: &str,
    ) -> Result<(), SnapshotError> {
        let unfound = || cursor.ids.iter().filter(|id| !cursor.found.contains(*id));
        let count = unfound().count();
        self.extend_projection_reservation(
            reservation,
            &cursor.context,
            deque_growth(&cursor.pending, count)
                + unfound()
                    .map(|id| located_item_bytes(id, status, None))
                    .sum::<usize>(),
        )?;
        let predicted = deque_capacity_for(&cursor.pending, count);
        cursor.pending.reserve(count);
        for id in &cursor.ids {
            if !cursor.found.contains(id) {
                cursor.pending.push_back(
                    json!({"session_id":id,"status":status,"path":null,"revision":null}),
                );
            }
        }
        assert_eq!(cursor.pending.capacity(), predicted);
        Ok(())
    }

    fn forget_location(
        &self,
        cursor: &mut LocateCursor,
        reservation: &mut ProjectionReservation<'_>,
        id: &str,
    ) -> Result<(), SnapshotError> {
        self.extend_projection_reservation(
            reservation,
            &cursor.context,
            cursor.found.len() * size_of::<String>(),
        )?;
        let capacity = cursor.found.capacity();
        let mut kept = Vec::with_capacity(cursor.found.len());
        kept.extend(cursor.found.drain().filter(|found| found.as_str() != id));
        cursor.found.extend(kept);
        assert_eq!(cursor.found.capacity(), capacity);
        Ok(())
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
        #[cfg(test)]
        self.location_canonicalizations
            .fetch_add(1, Ordering::Relaxed);
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

    fn remember_locations<'a>(
        &self,
        entries: impl Clone + ExactSizeIterator<Item = (&'a String, &'a PathBuf)>,
        context: &Value,
    ) {
        if entries.len() == 0 {
            return;
        }
        let mut state = self.lock_state();
        Self::prune(&mut state);
        let remembered = || {
            entries
                .clone()
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
        usage: &mut [u64; 20],
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
        let id_bytes = requested
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let id = value
                    .as_str()
                    .ok_or_else(|| invalid("invalid session id"))?;
                if id.is_empty()
                    || id.len() > 256
                    || requested
                        .iter()
                        .take(index)
                        .any(|prior| prior.as_str() == Some(id))
                {
                    return Err(invalid("invalid or duplicate session id"));
                }
                Ok(id.len())
            })
            .sum::<Result<usize, _>>()?;
        let roots = request
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing roots"))?;
        if roots.is_empty() || roots.len() > 64 {
            return Err(invalid("invalid root count"));
        }
        let claimant = str_field(context, "claimant")?;
        let paths = roots.len() * size_of::<PathBuf>()
            + roots
                .iter()
                .map(|root| root.as_str().map_or(0, str::len))
                .sum::<usize>();
        let mut reservation = self.reserve_projection(
            context,
            size_of::<LocateCursor>()
                + claimant.len()
                + value_bytes(context)
                + requested.len() * size_of::<String>()
                + set_growth(&HashSet::<String>::new(), requested.len())
                + 2 * id_bytes
                + 2 * paths,
        )?;
        #[cfg(test)]
        self.locate_records.fetch_add(1, Ordering::Relaxed);
        let wanted_capacity = set_capacity_for(&HashSet::<String>::new(), requested.len());
        let mut ids = Vec::with_capacity(requested.len());
        let mut wanted = HashSet::with_capacity(requested.len());
        for value in requested.iter() {
            let id = value.as_str().expect("validated session id");
            wanted.insert(id.to_owned());
            ids.push(id.to_owned());
        }
        assert_eq!(
            (ids.capacity(), wanted.capacity()),
            (requested.len(), wanted_capacity)
        );
        self.extend_projection_reservation(
            &mut reservation,
            context,
            if context["authority"]["kind"].as_str() == Some("user") {
                LOCATE_USER_PATH_SLOTS
            } else {
                LOCATE_PATH_SLOTS
            },
        )?;
        let mut scope = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            cancel.check(bound.deadline_unix_ms)?;
            let path = PathBuf::from(root.as_str().ok_or_else(|| invalid("invalid root"))?);
            #[cfg(test)]
            self.location_canonicalizations
                .fetch_add(1, Ordering::Relaxed);
            let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
            self.authority(context, Some(&canonical))?;
            #[cfg(test)]
            self.after_location_metadata("root");
            scope.push(path);
        }
        let cached = {
            let mut state = self.lock_state();
            let (count, bytes) = ids
                .iter()
                .filter_map(|id| {
                    state
                        .locations
                        .get(id)
                        .map(|location| id.len() + location.path.as_os_str().len())
                })
                .fold((0, 0), |(count, bytes), hit| (count + 1, bytes + hit));
            self.extend_projection_reservation_in(
                &mut state,
                &mut reservation,
                context,
                count * size_of::<(String, PathBuf)>() + bytes,
            )?;
            let mut cached = Vec::with_capacity(count);
            cached.extend(ids.iter().filter_map(|id| {
                state
                    .locations
                    .get(id)
                    .map(|location| (id.clone(), location.path.clone()))
            }));
            cached
        };
        let mut found = HashSet::new();
        let mut pending = VecDeque::new();
        let mut refreshed = Vec::new();
        for (id, path) in cached {
            cancel.check(bound.deadline_unix_ms)?;
            if let Some(stamp) = self.location_candidate(&path, &scope, context)? {
                #[cfg(test)]
                self.after_location_metadata("cached");
                let revision = stamp.revision();
                self.extend_projection_reservation(
                    &mut reservation,
                    context,
                    set_growth(&found, 1)
                        + id.len()
                        + deque_growth(&pending, 1)
                        + located_item_bytes(&id, "ok", Some((path.as_path(), revision.as_str())))
                        + vec_growth(&refreshed, 1),
                )?;
                #[cfg(test)]
                self.locate_items.fetch_add(1, Ordering::Relaxed);
                let predicted = (
                    set_capacity_for(&found, 1),
                    deque_capacity_for(&pending, 1),
                    vec_capacity_for(&refreshed, 1),
                );
                found.insert(id.clone());
                pending.push_back(json!({"session_id":id,"status":"ok","path":path.to_string_lossy().as_ref(),"revision":revision}));
                refreshed.push((id, path));
                assert_eq!(
                    (found.capacity(), pending.capacity(), refreshed.capacity()),
                    predicted
                );
            }
        }
        cancel.check(bound.deadline_unix_ms)?;
        self.remember_locations(refreshed.iter().map(|(id, path)| (id, path)), context);
        drop(refreshed);
        let token = self.token("location");
        let cursor = LocateCursor {
            claimant: claimant.to_owned(),
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
        self.locate_step(&token, cursor, &mut reservation, cancel, usage)
    }

    fn locate_step(
        &self,
        token: &str,
        mut cursor: LocateCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(cursor.limits.deadline_unix_ms)?;
        let mut output = Vec::new();
        let mut page_bytes = 64usize;
        let stop = cursor
            .examined
            .saturating_add(self.config.event_step)
            .min(cursor.limits.max_discovery_entries);
        let mut updates = Vec::new();
        let mut parent_hint: Option<(PathBuf, Option<PathBuf>)> = None;
        loop {
            cancel.check(cursor.limits.deadline_unix_ms)?;
            while let Some(mut item) = cursor.pending.pop_front() {
                cancel.check(cursor.limits.deadline_unix_ms)?;
                if item["status"].as_str() == Some("ok") {
                    let Some(stamp) = self.location_candidate(
                        Path::new(str_field(&item, "path")?),
                        &cursor.scope,
                        &cursor.context,
                    )?
                    else {
                        self.forget_location(
                            &mut cursor,
                            reservation,
                            str_field(&item, "session_id")?,
                        )?;
                        continue;
                    };
                    #[cfg(test)]
                    self.after_location_metadata("pending");
                    let revision = stamp.revision();
                    self.extend_projection_reservation(
                        reservation,
                        &cursor.context,
                        revision.len(),
                    )?;
                    item.insert("revision", json!(revision));
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
                            let path = entry.path();
                            #[cfg(test)]
                            self.after_location_metadata("entry");
                            if cursor.context["authority"]["kind"].as_str() == Some("user") {
                                let parent = path.parent().expect("directory entry parent");
                                if parent_hint.as_ref().is_none_or(|(raw, _)| raw != parent) {
                                    drop(parent_hint.take());
                                    #[cfg(test)]
                                    self.location_canonicalizations
                                        .fetch_add(1, Ordering::Relaxed);
                                    let resolved = std::fs::canonicalize(parent).ok();
                                    parent_hint = Some((parent.to_path_buf(), resolved));
                                }
                                let metadata = parent_hint.as_mut().and_then(|(_raw, resolved)| {
                                    let child = resolved.as_mut()?;
                                    let leaf = path.file_name().expect("directory entry name");
                                    child.reserve_exact(1 + leaf.len());
                                    child.push(leaf);
                                    #[cfg(test)]
                                    assert!(
                                        path.capacity()
                                            + _raw.capacity()
                                            + child.capacity()
                                            + (path.as_os_str().len() + 1)
                                                .max(child.as_os_str().len() + 1)
                                            + size_of::<libc::dirent>()
                                            <= LOCATE_USER_PATH_SLOTS
                                    );
                                    let metadata = std::fs::symlink_metadata(&*child).ok();
                                    child.pop();
                                    let metadata = metadata?;
                                    #[cfg(test)]
                                    self.after_location_metadata("canonical_prefilter");
                                    let raw_metadata = std::fs::symlink_metadata(&path).ok()?;
                                    (metadata.is_file()
                                        && raw_metadata.is_file()
                                        && SourceStamp::of(&metadata).identity
                                            == SourceStamp::of(&raw_metadata).identity)
                                        .then_some(metadata)
                                });
                                if let Some(metadata) = metadata {
                                    #[cfg(test)]
                                    self.after_location_metadata("prefilter");
                                    if metadata.is_file()
                                        && (path.extension().is_none_or(|ext| ext != "jsonl")
                                            || Self::requested_location(
                                                &path
                                                    .file_stem()
                                                    .expect("source filename")
                                                    .to_string_lossy(),
                                                &cursor.wanted,
                                            )
                                            .is_none())
                                    {
                                        continue;
                                    }
                                }
                            }
                            parent_hint = None;
                            path
                        }
                        Some(Err(error)) => return Err(io_error(error)),
                        None => {
                            cursor.directories.pop();
                            parent_hint = None;
                            continue;
                        }
                    }
                } else if let Some(root) = cursor.roots.pop() {
                    parent_hint = None;
                    root
                } else {
                    cursor.finished = true;
                    self.queue_unfound_locations(&mut cursor, reservation, "missing")?;
                    continue;
                };
                let metadata = {
                    #[cfg(test)]
                    if !cursor.directories.is_empty() {
                        self.location_entry_canonicalizations
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    #[cfg(test)]
                    self.location_canonicalizations
                        .fetch_add(1, Ordering::Relaxed);
                    let canonical = match std::fs::canonicalize(&path) {
                        Ok(path) => path,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(io_error(error)),
                    };
                    self.authority(&cursor.context, Some(&canonical))?;
                    match std::fs::metadata(&canonical) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(io_error(error)),
                    }
                };
                #[cfg(test)]
                self.after_location_metadata("walk");
                if metadata.is_dir() {
                    let identity = SourceStamp::of(&metadata).identity;
                    if !cursor.seen_directories.contains(&identity) {
                        self.extend_projection_reservation(
                            reservation,
                            &cursor.context,
                            set_growth(&cursor.seen_directories, 1)
                                + vec_growth(&cursor.directories, 1)
                                + READ_DIR_HANDLE_BYTES
                                + path.as_os_str().len(),
                        )?;
                        let predicted = (
                            set_capacity_for(&cursor.seen_directories, 1),
                            vec_capacity_for(&cursor.directories, 1),
                        );
                        cursor.seen_directories.insert(identity);
                        cursor.directories.push(OpenDirectory::open(&path)?);
                        #[cfg(test)]
                        self.directory_opens.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(
                            (
                                cursor.seen_directories.capacity(),
                                cursor.directories.capacity()
                            ),
                            predicted
                        );
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
                if !cursor.found.contains(id) {
                    let stamp = SourceStamp::of(&metadata);
                    let revision = stamp.revision();
                    self.extend_projection_reservation(
                        reservation,
                        &cursor.context,
                        set_growth(&cursor.found, 1)
                            + id.len()
                            + deque_growth(&cursor.pending, 1)
                            + located_item_bytes(
                                id,
                                "ok",
                                Some((path.as_path(), revision.as_str())),
                            )
                            + vec_growth(&updates, 1)
                            + id.len()
                            + path.capacity(),
                    )?;
                    #[cfg(test)]
                    self.locate_items.fetch_add(1, Ordering::Relaxed);
                    let predicted = (
                        deque_capacity_for(&cursor.pending, 1),
                        vec_capacity_for(&updates, 1),
                    );
                    cursor.found.insert(id.to_owned());
                    cursor.pending.push_back(json!({"session_id":id,"status":"ok","path":path.to_string_lossy().as_ref(),"revision":revision}));
                    updates.push((id.to_owned(), path));
                    assert_eq!((cursor.pending.capacity(), updates.capacity()), predicted);
                }
                continue;
            }
            let status = if cursor.exhausted {
                "incomplete"
            } else {
                "missing"
            };
            self.queue_unfound_locations(&mut cursor, reservation, status)?;
        }
        cancel.check(cursor.limits.deadline_unix_ms)?;
        self.remember_locations(updates.iter().map(|(id, path)| (id, path)), &cursor.context);
        drop(updates);
        cancel.check(cursor.limits.deadline_unix_ms)?;
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
        cancel.check(cursor.limits.deadline_unix_ms)?;
        Self::prune(&mut state);
        if state.locates.len() >= self.lease_cap(&cursor.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "location cursor admission exhausted",
            ));
        }
        cursor.expires = (now_ms() + self.config.ttl).min(cursor.limits.deadline_unix_ms);
        let token = token.to_owned();
        let pledge = Delivery::cursor_pledge(&cursor.claimant, &token);
        let additional =
            state.admission(&token, &cursor, []) + state.locates.growth_for(&token) + pledge;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, &cursor.context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.locates.reserve_for(&token);
        state.locates.insert(token.clone(), cursor);
        state.locates.pledge(&token, pledge);
        drop(state);
        self.after_location_park();
        Ok((
            data,
            Some(token),
            Some("location page incomplete".to_owned()),
        ))
    }

    fn resolve(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
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
        let id_bytes = ids
            .iter()
            .map(|id| {
                id.as_str()
                    .map(str::len)
                    .ok_or_else(|| invalid("invalid session id"))
            })
            .sum::<Result<usize, _>>()?;
        let claimant = str_field(context, "claimant")?;
        let mut reservation = self.reserve_projection(
            context,
            claimant.len()
                + value_bytes(context)
                + value_bytes(request)
                + ids.len() * size_of::<String>()
                + id_bytes,
        )?;
        #[cfg(test)]
        self.resolution_records.fetch_add(1, Ordering::Relaxed);
        let mut wanted = HashSet::new();
        for id in ids.iter() {
            wanted.insert(id.as_str().expect("validated session id").to_owned());
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
                    let (table, growth) = if found.contains_key(id) {
                        (found.capacity(), 0)
                    } else {
                        (
                            map_capacity_for(&found, 1),
                            map_growth(&found, 1) + id.len(),
                        )
                    };
                    self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        growth + path.capacity(),
                    )?;
                    found.insert(id.clone(), path);
                    assert_eq!(found.capacity(), table);
                }
            }
        }
        let token = self.token("resolution");
        let mut session_ids = Vec::with_capacity(ids.len());
        session_ids.extend(
            ids.iter()
                .map(|id| id.as_str().expect("validated id").to_owned()),
        );
        assert_eq!(session_ids.capacity(), ids.len());
        let cursor = ResolutionCursor {
            claimant: claimant.to_owned(),
            context: context.clone(),
            request: request.clone(),
            ids: session_ids,
            paths: found,
            sessions: Vec::new(),
            next: 0,
            pending: None,
            remaining: bound,
            complete_scan: complete,
            expires: (now_ms() + self.config.ttl).min(bound.deadline_unix_ms),
        };
        self.resolve_step(&token, cursor, &mut reservation, cancel, usage)
    }

    fn resolve_step(
        &self,
        token: &str,
        mut cursor: ResolutionCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let delivered = cursor.sessions.len();
        let stepped = self.resolution_steps(&mut cursor, reservation, cancel, usage);
        let pending = cursor.pending.clone();
        let issued: Vec<String> = cursor.sessions[delivered..]
            .iter()
            .filter(|session| session["status"].as_str() == Some("ok"))
            .filter_map(|session| session["description"]["handle"]["lease_id"].as_str())
            .map(str::to_owned)
            .collect();
        let reply = stepped.and_then(|()| self.resolution_page(token, cursor, reservation));
        if reply.is_err() {
            let mut state = self.lock_state();
            if let Some(pending) = &pending {
                state.waiters.remove(pending);
            }
            for lease in &issued {
                state.leases.remove(lease);
            }
            Self::prune(&mut state);
        }
        reply
    }

    fn resolution_steps(
        &self,
        cursor: &mut ResolutionCursor,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(), SnapshotError> {
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
                let session = json!({"session_id":id,"status":if cursor.complete_scan {"missing"} else {"incomplete"},"description":null});
                self.extend_projection_reservation(
                    reservation,
                    &cursor.context,
                    vec_growth(&cursor.sessions, 1) + value_bytes(&session),
                )?;
                let predicted = vec_capacity_for(&cursor.sessions, 1);
                cursor.sessions.push(session);
                assert_eq!(cursor.sessions.capacity(), predicted);
                cursor.next += 1;
                continue;
            };
            let before_read = usage[1];
            let before_events = usage[3];
            let outcome = if let Some(pending) = &cursor.pending {
                let expired =
                    || SnapshotError::new(Status::StaleCursor, "resolution reservation expired");
                if !self.lock_state().waiters.contains_key(pending) {
                    return Err(expired());
                }
                self.authority(&cursor.context, Some(path))?;
                let waiter = self
                    .rebind_waiter(&mut self.lock_state(), pending, &cursor.context)?
                    .ok_or_else(expired)?;
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
                let adopted = self
                    .pin_scope_for_work(handle, &cursor.context, cursor.remaining.deadline_unix_ms)
                    .and_then(|(resolved, description)| {
                        if resolved.session_id == *id {
                            Ok(description)
                        } else {
                            Err(SnapshotError::new(
                                Status::Changed,
                                "candidate source session differs from requested identity",
                            ))
                        }
                    });
                if adopted.is_err() {
                    self.lock_state()
                        .leases
                        .remove(str_field(handle, "lease_id")?);
                }
                let description = adopted?;
                let session = json!({"session_id":id,"status":"ok","description":description});
                let admitted = self.extend_projection_reservation(
                    reservation,
                    &cursor.context,
                    vec_growth(&cursor.sessions, 1) + value_bytes(&session),
                );
                if admitted.is_err() {
                    self.lock_state()
                        .leases
                        .remove(str_field(handle, "lease_id")?);
                }
                admitted?;
                let predicted = vec_capacity_for(&cursor.sessions, 1);
                cursor.sessions.push(session);
                assert_eq!(cursor.sessions.capacity(), predicted);
                cursor.next += 1;
            }
            break;
        }
        Ok(())
    }

    fn resolution_page(
        &self,
        token: &str,
        mut cursor: ResolutionCursor,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
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
        cursor.expires = (now_ms() + self.config.ttl).min(cursor.remaining.deadline_unix_ms);
        let token = token.to_owned();
        let pledge = Delivery::cursor_pledge(&cursor.claimant, &token);
        let additional =
            state.admission(&token, &cursor, []) + state.resolutions.growth_for(&token) + pledge;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, &cursor.context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.resolutions.reserve_for(&token);
        state.resolutions.insert(token.clone(), cursor);
        state.resolutions.pledge(&token, pledge);
        Ok((
            data,
            Some(token),
            Some("resolution preparation incomplete".to_owned()),
        ))
    }
}

include!("snapshot_catalog.rs");
include!("snapshot_symlink_proof.rs");
include!("snapshot_catalog_owner.rs");
include!("snapshot_prepared_service.rs");
include!("snapshot_cache.rs");
include!("snapshot_root_warm.rs");
include!("snapshot_codex_append.rs");
include!("snapshot_spine.rs");
include!("snapshot_raw_scope.rs");
include!("snapshot_raw_materialization.rs");
include!("snapshot_raw_projection.rs");
include!("snapshot_interactions.rs");
include!("snapshot_interaction_api.rs");

#[cfg(test)]
#[path = "snapshot_regressions.rs"]
mod regression_tests;

#[cfg(test)]
#[path = "snapshot_ledger_tests.rs"]
mod ledger_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_prepared::TextState;
    use std::io::Write;

    include!("snapshot_admission_report_tests.rs");

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

    include!("snapshot_raw_scope_tests.rs");
    include!("snapshot_raw_projection_tests.rs");
    include!("snapshot_raw_tail_tests.rs");
    include!("snapshot_interactions_tests.rs");
    include!("snapshot_interaction_api_tests.rs");
    include!("snapshot_catalog_tests.rs");
    include!("snapshot_common_borrowed_parent_tests.rs");

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

    #[cfg(target_os = "macos")]
    #[test]
    fn location_preserves_reachable_alias_canonicalization_errors() {
        let source = Source::new("");
        let mut root = source.directory.clone();
        let expanded = std::fs::canonicalize(&root).unwrap();
        let target_bytes = libc::PATH_MAX as usize - 8;
        let mut remaining = target_bytes - expanded.as_os_str().len() - 4;
        for depth in 0..4 {
            let width = remaining / (4 - depth);
            assert!((1..=255).contains(&width));
            remaining -= width;
            let name = "d".repeat(width);
            std::fs::create_dir(root.join(&name)).unwrap();
            std::os::unix::fs::symlink(&name, root.join("a")).unwrap();
            root.push("a");
        }
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        assert_eq!(canonical_root.as_os_str().len(), target_bytes);
        let entry = root.join("unrelated.txt");
        std::fs::write(&entry, b"").unwrap();
        assert!(std::fs::symlink_metadata(&entry).unwrap().is_file());
        let error = std::fs::canonicalize(&entry).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENAMETOOLONG));
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate-pathmax","operation":"locate","session_ids":["wanted"],"roots":[root],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            response["status"].as_str(),
            Some("changed"),
            "raw_entry={entry:?}, canonical_root={canonical_root:?}, canonical_error={error:?}, response={response:?}"
        );
        store.assert_conserved();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn location_preserves_oversized_raw_alias_entry_errors() {
        let source = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        let physical_entry = source.directory.join("unrelated.txt");
        std::fs::write(&physical_entry, b"").unwrap();
        let target_bytes = libc::PATH_MAX as usize - 8;
        let mut spelling = source.directory.to_string_lossy().into_owned();
        spelling.push_str(&"/".repeat(target_bytes - spelling.len()));
        let root = PathBuf::from(spelling);
        assert_eq!(root.as_os_str().len(), target_bytes);
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let entry = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(entry.as_os_str().len() >= libc::PATH_MAX as usize);
        assert!(
            std::fs::symlink_metadata(canonical_root.join("unrelated.txt"))
                .unwrap()
                .is_file()
        );
        assert_eq!(
            std::fs::symlink_metadata(&entry)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENAMETOOLONG)
        );
        assert_eq!(
            std::fs::canonicalize(&entry).unwrap_err().raw_os_error(),
            Some(libc::ENAMETOOLONG)
        );
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate-raw-pathmax","operation":"locate","session_ids":["wanted"],"roots":[root],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("changed"), "{response:?}");
        store.assert_conserved();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn location_dual_proof_admits_long_path_scratch() {
        let source = Source::new("");
        let mut root = source.directory.clone();
        let target = libc::PATH_MAX as usize - 96;
        while root.as_os_str().len() < target {
            let remaining = target - root.as_os_str().len();
            let width = if remaining > 256 { 240 } else { remaining - 1 };
            root.push("d".repeat(width));
            std::fs::create_dir(&root).unwrap();
        }
        assert_eq!(root.as_os_str().len(), target);
        let name = "n".repeat(20);
        std::fs::write(root.join(&name), b"").unwrap();
        let entry = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut child = std::fs::canonicalize(&root).unwrap();
        child.reserve_exact(1 + name.len());
        child.push(&name);
        let c_path = std::ffi::CString::new(child.as_os_str().as_encoded_bytes()).unwrap();
        let peak = entry.capacity()
            + root.as_os_str().len()
            + child.capacity()
            + c_path.as_bytes_with_nul().len()
            + size_of::<libc::dirent>();
        assert!(peak > LOCATE_PATH_SLOTS);
        assert!(peak <= LOCATE_USER_PATH_SLOTS);
        drop(c_path);
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate-long-scratch","operation":"locate","session_ids":["wanted"],"roots":[root],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
        assert_eq!(store.location_canonicalizations.load(Ordering::Relaxed), 3);
        store.assert_conserved();
    }

    #[test]
    fn location_dual_proof_rejects_different_file_identities() {
        let source = Source::new("");
        let old = source.directory.join("old");
        let new = source.directory.join("new");
        let alias = source.directory.join("alias");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        for parent in [&old, &new] {
            std::fs::write(parent.join("unrelated.txt"), b"").unwrap();
        }
        std::os::unix::fs::symlink(&old, &alias).unwrap();
        let store = store();
        let switched = alias.clone();
        *store.location_metadata_hook.lock().unwrap() = Some((
            "canonical_prefilter",
            Arc::new(move || {
                std::fs::remove_file(&switched).unwrap();
                std::os::unix::fs::symlink(&new, &switched).unwrap();
            }),
        ));
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[alias],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
        assert_eq!(
            store
                .location_entry_canonicalizations
                .load(Ordering::Relaxed),
            1
        );
        store.assert_conserved();
    }

    #[test]
    fn location_nonmatching_regular_files_share_parent_canonicalization() {
        let source = Source::new("");
        std::fs::remove_file(&source.path).unwrap();
        for index in 0..20 {
            std::fs::write(
                source.directory.join(format!("unrelated-{index}.jsonl")),
                b"",
            )
            .unwrap();
        }
        let store = NativeStore::new(&json!({"max_events_per_step":64})).unwrap();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["absent"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
        assert_eq!(store.lock_state().counters[17], 20);
        assert_eq!(store.location_canonicalizations.load(Ordering::Relaxed), 3);
        store.assert_conserved();
        assert_eq!(
            store
                .location_entry_canonicalizations
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn location_parent_hint_refreshes_between_pages() {
        for step in [2, 64] {
            let expected = "ok";
            let source = Source::new("");
            let old = source.directory.join("old");
            let new = source.directory.join("new");
            let alias = source.directory.join("alias");
            std::fs::create_dir(&old).unwrap();
            std::fs::create_dir(&new).unwrap();
            for index in 0..4 {
                let name = format!("unrelated-{index}.txt");
                std::fs::write(old.join(&name), b"").unwrap();
                std::fs::create_dir(new.join(&name)).unwrap();
                std::fs::write(new.join(name).join("wanted.jsonl"), b"").unwrap();
            }
            std::os::unix::fs::symlink(&old, &alias).unwrap();
            let store = NativeStore::new(&json!({"max_events_per_step":step})).unwrap();
            let switched = alias.clone();
            *store.location_metadata_hook.lock().unwrap() = Some((
                "prefilter",
                Arc::new(move || {
                    std::fs::remove_file(&switched).unwrap();
                    std::os::unix::fs::symlink(&new, &switched).unwrap();
                }),
            ));
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[alias],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let first = store.request(&request, &owner, &Cancellation::default());
            if step == 2 {
                assert!(first["cursor"].as_str().is_some(), "{first:?}");
                assert_eq!(store.lock_state().counters[17], 2);
                assert_eq!(store.location_canonicalizations.load(Ordering::Relaxed), 4);
                store.assert_conserved();
            }
            let response = finish(&store, first, &owner);
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            let item = &response["data"]["sessions"][0];
            assert_eq!(item["status"].as_str(), Some(expected), "{response:?}");
            if expected == "ok" {
                let path = Path::new(item["path"].as_str().unwrap());
                assert!(path.starts_with(&alias));
                assert_eq!(path.file_name(), Some(OsStr::new("wanted.jsonl")));
            }
            store.assert_conserved();
        }
    }

    #[test]
    fn location_parent_hint_counts_each_page_and_ignores_empty_directories() {
        for count in [0, 20] {
            let source = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            for index in 0..count {
                std::fs::write(source.directory.join(format!("unrelated-{index}.txt")), b"")
                    .unwrap();
            }
            let store = store();
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
            assert_eq!(store.lock_state().counters[17], count);
            assert_eq!(
                store.location_canonicalizations.load(Ordering::Relaxed),
                2 + count as usize / 2
            );
            store.assert_conserved();
        }
    }

    #[test]
    fn location_parent_hint_failure_defers_to_raw_entry_error() {
        let source = Source::new("");
        let alias = source.directory.join("alias");
        let physical = source.directory.join("physical");
        std::fs::create_dir(&physical).unwrap();
        std::fs::write(physical.join("unrelated.txt"), b"").unwrap();
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        let store = store();
        let switched = alias.clone();
        *store.location_metadata_hook.lock().unwrap() = Some((
            "entry",
            Arc::new(move || {
                std::fs::remove_file(&switched).unwrap();
                std::os::unix::fs::symlink("alias", &switched).unwrap();
            }),
        ));
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[alias],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("changed"), "{response:?}");
        assert_eq!(store.lock_state().counters[17], 1);
        assert_eq!(
            store
                .location_entry_canonicalizations
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(store.location_canonicalizations.load(Ordering::Relaxed), 4);
        store.assert_conserved();
    }

    #[test]
    fn location_prefilter_preserves_filename_matches_and_nonmatching_directories() {
        for (filename, id, found) in [
            ("wanted.jsonl", "wanted", true),
            ("rollout-prefix-wanted.jsonl", "wanted", true),
            ("rollout-prefix-left-right.jsonl", "left-right", true),
            ("wanted.JSONL", "wanted", false),
            ("wanted.txt", "wanted", false),
        ] {
            let source = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            let nested = source.directory.join("unrelated");
            std::fs::create_dir(&nested).unwrap();
            let path = nested.join(filename);
            std::fs::write(&path, b"").unwrap();
            let store = store();
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":[id],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            let item = &response["data"]["sessions"][0];
            assert_eq!(item["status"], json!(if found { "ok" } else { "missing" }));
            if found {
                assert_eq!(item["path"], json!(path));
                assert_eq!(
                    item["revision"],
                    json!(SourceStamp::of(&std::fs::metadata(&path).unwrap()).revision())
                );
            }
            store.assert_conserved();
        }
    }

    #[test]
    fn location_prefilter_preserves_symlink_aliases_and_ordered_root_winners() {
        let source = Source::new("");
        let outside = Source::new("");
        let physical = outside.directory.join("physical.txt");
        std::fs::write(&physical, b"").unwrap();
        let alias = source.directory.join("arbitrary-prefix-wanted.jsonl");
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        let ancestor = outside.directory.join("ancestor");
        std::os::unix::fs::symlink(&source.directory, &ancestor).unwrap();
        let duplicate = outside.directory.join("wanted.jsonl");
        std::fs::write(&duplicate, b"").unwrap();
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[duplicate,ancestor],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        let item = &response["data"]["sessions"][0];
        assert_eq!(
            item["path"],
            json!(ancestor.join("arbitrary-prefix-wanted.jsonl"))
        );
        assert_eq!(
            item["revision"],
            json!(SourceStamp::of(&std::fs::metadata(&physical).unwrap()).revision())
        );
        store.assert_conserved();
    }

    #[test]
    fn location_prefilter_preserves_nonmatching_symlink_errors() {
        for (target, status) in [("unrelated.txt", "changed"), ("missing", "ok")] {
            let source = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            std::os::unix::fs::symlink(target, source.directory.join("unrelated.txt")).unwrap();
            let store = store();
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some(status), "{response:?}");
            assert_eq!(
                store
                    .location_entry_canonicalizations
                    .load(Ordering::Relaxed),
                1
            );
            store.assert_conserved();
        }
    }

    #[test]
    fn location_prefilter_keeps_restricted_authority_exact() {
        for outside_link in [false, true] {
            let source = Source::new("");
            let outside = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            let path = source.directory.join("unrelated.txt");
            if outside_link {
                std::os::unix::fs::symlink(&outside.path, &path).unwrap();
            } else {
                std::fs::write(&path, b"").unwrap();
            }
            let store = store();
            let mut owner = context("a");
            owner.insert("authority", json!({"kind":"restricted_roots","roots":[source.directory],"effective_uid":unsafe { libc::geteuid() }.to_string()}));
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(
                response["status"],
                json!(if outside_link {
                    "permission_denied"
                } else {
                    "ok"
                }),
                "{response:?}"
            );
            assert_eq!(
                store
                    .location_entry_canonicalizations
                    .load(Ordering::Relaxed),
                1
            );
            store.assert_conserved();
        }
    }

    #[test]
    fn location_prefilter_keeps_special_files_and_failed_prechecks_exact() {
        for removed in [false, true] {
            let source = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            let path = source.directory.join("unrelated.txt");
            if removed {
                std::fs::write(&path, b"").unwrap();
            } else {
                let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            let store = store();
            if removed {
                *store.location_metadata_hook.lock().unwrap() = Some((
                    "entry",
                    Arc::new(move || std::fs::remove_file(&path).unwrap()),
                ));
            }
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
            assert_eq!(
                store
                    .location_entry_canonicalizations
                    .load(Ordering::Relaxed),
                1
            );
            store.assert_conserved();
        }
    }

    #[test]
    fn location_prefilter_classifies_at_first_metadata_observation() {
        for directory in [false, true] {
            let source = Source::new("");
            std::fs::remove_file(&source.path).unwrap();
            let path = source.directory.join("unrelated.txt");
            std::fs::write(&path, b"").unwrap();
            let store = store();
            *store.location_metadata_hook.lock().unwrap() = Some((
                "prefilter",
                Arc::new(move || {
                    std::fs::remove_file(&path).unwrap();
                    if directory {
                        std::fs::create_dir(&path).unwrap();
                        std::fs::write(path.join("wanted.jsonl"), b"").unwrap();
                    } else {
                        std::os::unix::fs::symlink("unrelated.txt", &path).unwrap();
                    }
                }),
            ));
            let owner = context("a");
            let template = acquire(&source.path);
            let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let response = finish(
                &store,
                store.request(&request, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(response["data"]["sessions"][0]["status"], json!("missing"));
            assert_eq!(store.lock_state().counters[17], 1);
            assert_eq!(
                store
                    .location_entry_canonicalizations
                    .load(Ordering::Relaxed),
                0
            );
            store.assert_conserved();
        }
    }

    #[test]
    fn location_prefilter_cancellation_stops_before_the_next_entry() {
        let source = Source::new("");
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let cancel = Cancellation::default();
        let stopped = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() =
            Some(("prefilter", Arc::new(move || stopped.cancel())));
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["wanted"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = store.request(&request, &owner, &cancel);
        assert_eq!(
            response["status"].as_str(),
            Some("cancelled"),
            "{response:?}"
        );
        assert!(response["cursor"].is_null());
        assert!(store.lock_state().locates.is_empty());
        assert_eq!(store.lock_state().counters[17], 1);
        assert_eq!(
            store
                .location_entry_canonicalizations
                .load(Ordering::Relaxed),
            0
        );
        store.assert_conserved();
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
    fn location_cancellation_stops_before_the_next_root() {
        let source = Source::new("");
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let cancel = Cancellation::default();
        let stopped = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() =
            Some(("root", Arc::new(move || stopped.cancel())));
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["absent"],"roots":[source.directory,source.directory.join("missing")],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = store.request(&request, &owner, &cancel);
        assert_eq!(
            response["status"].as_str(),
            Some("cancelled"),
            "{response:?}"
        );
        assert!(response["cursor"].is_null());
        assert_eq!(response["usage"]["discovery_entries_examined"], json!(0));
    }

    #[test]
    fn location_cancellation_does_not_refresh_cached_locations() {
        let source = Source::new("");
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let expires = now_ms() + 100_000;
        store.lock_state().insert_location(
            "s".to_owned(),
            LocatedPath {
                path: source.path.clone(),
                expires,
            },
        );
        let cancel = Cancellation::default();
        let stopped = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() =
            Some(("cached", Arc::new(move || stopped.cancel())));
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["s"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = store.request(&request, &owner, &cancel);
        assert_eq!(
            response["status"].as_str(),
            Some("cancelled"),
            "{response:?}"
        );
        assert_eq!(
            store.lock_state().locations.get("s").unwrap().expires,
            expires
        );
        assert!(response["cursor"].is_null());
        assert_eq!(response["usage"]["discovery_entries_examined"], json!(0));
    }

    #[test]
    fn location_cancellation_stops_inside_recursive_metadata_work() {
        let source = Source::new("");
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let cancel = Cancellation::default();
        let stopped = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() =
            Some(("walk", Arc::new(move || stopped.cancel())));
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["absent"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let response = store.request(&request, &owner, &cancel);
        assert_eq!(
            response["status"].as_str(),
            Some("cancelled"),
            "{response:?}"
        );
        assert!(response["cursor"].is_null());
        assert_eq!(response["usage"]["discovery_entries_examined"], json!(0));
        assert!(store.lock_state().locates.is_empty());
    }

    #[test]
    fn location_cancellation_prevents_final_pending_publication() {
        let source = Source::new("");
        let store = store();
        let owner = context("a");
        let template = acquire(&source.path);
        let request = json!({"schema":SCHEMA,"id":"locate","operation":"locate","session_ids":["s"],"roots":[source.directory],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let first = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let cancel = Cancellation::default();
        let stopped = cancel.clone();
        *store.location_metadata_hook.lock().unwrap() =
            Some(("pending", Arc::new(move || stopped.cancel())));
        let response = store.request(&request, &owner, &cancel);
        assert_eq!(
            response["status"].as_str(),
            Some("cancelled"),
            "{response:?}"
        );
        assert!(response["cursor"].is_null());
        assert!(response["data"].is_null());
        assert_eq!(response["usage"]["discovery_entries_examined"], json!(0));
        assert!(store.lock_state().locates.is_empty());
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
    #[test]
    fn turn_projection_cursor_preserves_native_page_status_and_zero_item_incompleteness() {
        let source = Source::new(
            &(0..600)
                .map(|index| format!("{}\n", user(&format!("u{index}"))))
                .collect::<String>(),
        );
        let store = NativeStore::new(&json!({})).unwrap();
        let owner = context("turn-pages");
        let cancel = Cancellation::default();
        let mut acquiring = acquire(&source.path);
        acquiring["limits"].insert("max_events", json!(10000));
        let root = finish(&store, store.request(&acquiring, &owner, &cancel), &owner);
        assert_eq!(root["status"].as_str(), Some("ok"));
        for reverse in [false, true] {
            let mut req = graph_request(
                &root,
                json!({"kind":"turns","order":if reverse {"reverse"} else {"forward"}}),
                Vec::new(),
                json!([]),
            );
            req["limits"].insert("max_events", json!(10000));
            req["limits"].insert("max_items", json!(600));
            req["limits"].insert("max_read_bytes", json!(16 * 1024 * 1024));
            let mut response = store.request(&req, &owner, &cancel);
            for (page_index, expected_items) in [256, 256, 88].into_iter().enumerate() {
                assert_eq!(
                    response["data"]["records_json"].as_array().unwrap().len(),
                    expected_items
                );
                if page_index == 2 {
                    assert_eq!(response["status"].as_str(), Some("ok"));
                    assert!(response["cursor"].is_null());
                    assert!(response["reason"].is_null());
                } else {
                    assert_eq!(response["status"].as_str(), Some("incomplete"));
                    assert_eq!(response["reason"].as_str(), Some("item_limit"));
                    let cursor = response["cursor"].as_str().unwrap().to_owned();
                    response = store.request(
                        &json!({"schema":SCHEMA,"id":"turn-resume","operation":"resume","cursor":cursor}),
                        &owner,
                        &cancel,
                    );
                }
            }
            req["limits"].insert("max_items", json!(0));
            let zero = store.request(&req, &owner, &cancel);
            assert_eq!(zero["status"].as_str(), Some("incomplete"));
            assert_eq!(zero["reason"].as_str(), Some("item_limit"));
            assert!(zero["cursor"].is_null());
            assert!(zero["data"]["records_json"].as_array().unwrap().is_empty());
        }
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

    fn tail_with_small_step_reference(source: &Source, count: usize, budget: usize) -> Value {
        let owner = context("a");
        let request = tail_request(&source.path, count, budget);
        let store = NativeStore::new(&json!({})).unwrap();
        let response = store.request(&request, &owner, &Cancellation::default());
        let reference = NativeStore::new(&json!({"max_read_bytes_per_step":4096})).unwrap();
        let reference = reference.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"], reference["status"], "{response:?}");
        assert_eq!(response["reason"], reference["reason"], "{response:?}");
        assert_eq!(response["data"], reference["data"], "{response:?}");
        response
    }

    #[test]
    fn tail_default_steps_read_only_the_newest_event_window() {
        let suffix = (0..256)
            .map(|index| format!("{}\n", user_text(&index.to_string(), &"x".repeat(1400))))
            .collect::<String>();
        assert!((256 * 1024..448 * 1024).contains(&suffix.len()));
        let prefix = format!("{}\n", " ".repeat(8 * 1024 * 1024));
        let source = Source::new(&format!("{prefix}{suffix}"));
        let response = tail_with_small_step_reference(&source, 256, 8 * 1024 * 1024);
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(
            tail_uuids(&response),
            (0..256).map(|index| index.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(
            response["data"]["source_bytes"].as_u64(),
            Some((prefix.len() + suffix.len()) as u64)
        );
        assert_eq!(
            response["data"]["window_start_byte"].as_u64(),
            Some(prefix.len() as u64)
        );
        let read = response["usage"]["source_bytes_read"].as_u64().unwrap();
        assert!(read >= suffix.len() as u64 && read <= 512 * 1024, "{read}");
    }

    #[test]
    fn tail_grows_reads_for_a_long_partial_line() {
        fn count_reads(store: &Arc<NativeStore>, reads: &Arc<AtomicUsize>) {
            let next = Arc::downgrade(store);
            let reads = Arc::clone(reads);
            *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
                reads.fetch_add(1, Ordering::Relaxed);
                count_reads(&next.upgrade().unwrap(), &reads);
            }));
        }

        let prefix = format!("{}\n", " ".repeat(8 * 1024 * 1024));
        let suffix = format!("{}\n", user_text("large", &"x".repeat(512 * 1024)));
        let source = Source::new(&format!("{prefix}{suffix}"));
        let owner = context("a");
        let request = tail_request(&source.path, 1, 8 * 1024 * 1024);
        let store = Arc::new(NativeStore::new(&json!({})).unwrap());
        let reads = Arc::new(AtomicUsize::new(0));
        count_reads(&store, &reads);
        let response = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(tail_uuids(&response), ["large"]);
        assert_eq!(
            response["data"]["window_start_byte"].as_u64(),
            Some(prefix.len() as u64)
        );
        assert!(response["usage"]["source_bytes_read"].as_u64().unwrap() <= 1024 * 1024);
        assert!(reads.load(Ordering::Relaxed) <= 4);
        let bounded = NativeStore::new(&json!({"max_entry_bytes":256*1024})).unwrap();
        let refused = bounded.request(&request, &owner, &Cancellation::default());
        assert_eq!(
            refused["status"].as_str(),
            Some("entry_limit"),
            "{refused:?}"
        );
    }

    #[test]
    fn tail_preserves_utf8_across_growing_read_boundaries() {
        let mut split_scalars = 0;
        for ending in ["", "\n"] {
            let contents = format!(
                "{}\n{}{ending}",
                user("old"),
                user_text("unicode", &"é".repeat(128 * 1024))
            );
            let boundary = contents.len() - 64 * 1024;
            split_scalars += usize::from(contents.as_bytes()[boundary] & 0xc0 == 0x80);
            let source = Source::new(&contents);
            let response = tail_with_small_step_reference(&source, 1, 1024 * 1024);
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            assert_eq!(tail_uuids(&response), ["unicode"]);
            assert_eq!(
                response["data"]["window_start_byte"].as_u64(),
                Some((user("old").len() + 1) as u64)
            );
        }
        assert_eq!(split_scalars, 1);
    }

    #[test]
    fn tail_read_budget_requires_the_oldest_line_delimiter() {
        let prefix = format!("{}\n", user("old"));
        let first = format!("{}\n", user_text("a", &"x".repeat(128 * 1024)));
        let last = format!("{}\n", user_text("b", &"x".repeat(128 * 1024)));
        let source = Source::new(&format!("{prefix}{first}{last}"));
        let budget = first.len() + last.len();
        let incomplete = tail_with_small_step_reference(&source, 2, budget);
        assert_eq!(
            incomplete["status"].as_str(),
            Some("incomplete"),
            "{incomplete:?}"
        );
        assert_eq!(
            incomplete["reason"].as_str(),
            Some("tail read budget exhausted")
        );
        assert!(incomplete["cursor"].is_null());
        assert_eq!(tail_uuids(&incomplete), ["b"]);
        assert_eq!(
            incomplete["data"]["window_start_byte"].as_u64(),
            Some((prefix.len() + first.len()) as u64)
        );
        let complete = tail_with_small_step_reference(&source, 2, budget + 1);
        assert_eq!(complete["status"].as_str(), Some("ok"), "{complete:?}");
        assert_eq!(tail_uuids(&complete), ["a", "b"]);
        assert_eq!(
            complete["data"]["window_start_byte"].as_u64(),
            Some(prefix.len() as u64)
        );
    }

    #[test]
    fn tail_counts_metadata_events_and_skips_non_events_across_steps() {
        let source = Source::new(&format!(
            "{}\n{}\nnot json\n[]\nnull\n42\n{{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}}\n{}\n",
            user("a"),
            " ".repeat(96 * 1024),
            user_text("b", &"x".repeat(128 * 1024)),
        ));
        let response = tail_with_small_step_reference(&source, 3, 1024 * 1024);
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        let records = response["data"]["records_json"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let records = crate::snapshot_codec::decode_events(&records, 1024 * 1024).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].event.meta().unwrap().uuid, "a");
        assert!(matches!(&records[1].event, Entry::Other(other) if other.ty == "queue-operation"));
        assert_eq!(records[2].event.meta().unwrap().uuid, "b");
        assert_eq!(response["data"]["window_start_byte"].as_u64(), Some(0));
    }

    #[test]
    fn tail_preserves_the_pinned_source_while_reads_grow() {
        let prefix = format!("{}\n", user("old"));
        let suffix = format!("{}\n", user_text("new", &"x".repeat(256 * 1024)));
        let source = Source::new(&format!("{prefix}{suffix}"));
        let path = source.path.clone();
        let store = NativeStore::new(&json!({})).unwrap();
        *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(format!("{}\n", user("appended")).as_bytes())
                .unwrap();
        }));
        let response = store.request(
            &tail_request(&source.path, 1, 1024 * 1024),
            &context("a"),
            &Cancellation::default(),
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(tail_uuids(&response), ["new"]);
        assert_eq!(
            response["data"]["source_bytes"].as_u64(),
            Some((prefix.len() + suffix.len()) as u64)
        );
        assert_eq!(
            response["data"]["window_start_byte"].as_u64(),
            Some(prefix.len() as u64)
        );
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
    fn tail_reads_the_newest_events_of_a_source_past_the_owner_bound() {
        let source = Source::new(&users(0..50));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":64,"max_source_bytes":users(40..50).len(),"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let response = store.request(
            &tail_request(&source.path, 5, 1024 * 1024),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        assert_eq!(tail_uuids(&response), ["45", "46", "47", "48", "49"]);
        let read = response["usage"]["source_bytes_read"].as_u64().unwrap() as usize;
        assert!(read <= users(40..50).len(), "{read}");
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
        assert_eq!(
            description["window_started_unix_ms"].as_u64(),
            Some(1_767_323_045_000)
        );
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
    fn windowed_alignment_reads_a_chunk_instead_of_the_whole_read_step() {
        let source = Source::new(&format!(
            "{}\n{}",
            user("0").replace("hello", &"x".repeat(9 * 1024 * 1024)),
            users(1..100)
        ));
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let owner = context("a");
        let (base, start, first) = expected_window(&source.path, 4096);
        assert!(base > 8 * 1024 * 1024);
        assert!(base - start < 1024);
        let mut request = windowed_acquire(&source.path, 4096);
        request["limits"].insert("max_source_read_bytes", json!(32 * 1024 * 1024));
        let response = finish(
            &store,
            store.request(&request, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        let snapshot = store.pin(handle(&response), &owner).unwrap();
        assert_eq!(snapshot.window_start, start);
        assert_eq!(snapshot.committed_bytes, snapshot.stamp.size);
        assert_eq!(uuids(&snapshot), (first..100).collect::<Vec<_>>());
        assert_eq!(snapshot.activity.turn_count(), 100 - first);
        let read = store.state.lock().unwrap().counters[1];
        assert_eq!(read, snapshot.stamp.size - start + 128 + 64 * 1024);
    }

    #[test]
    fn windowed_alignment_crosses_chunks_and_reaches_a_source_without_newlines() {
        for prefix in [false, true] {
            let source = Source::new(&format!(
                "{}{}",
                if prefix { users(0..2) } else { String::new() },
                user("2").replace("hello", &"x".repeat(2 * 1024 * 1024))
            ));
            let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_entry_bytes":4*1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
            let owner = context("a");
            let (base, start, _) = expected_window(&source.path, 1024);
            assert!(base - start > 128 * 1024);
            let mut request = windowed_acquire(&source.path, 1024);
            request["limits"].insert("max_source_read_bytes", json!(8 * 1024 * 1024));
            let mut response = store.request(&request, &owner, &Cancellation::default());
            for _ in 0..16 {
                if response["status"].as_str() != Some("incomplete") {
                    break;
                }
                response = store.request(
                    &json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":response["cursor"]}),
                    &owner,
                    &Cancellation::default(),
                );
            }
            assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
            let snapshot = store.pin(handle(&response), &owner).unwrap();
            assert_eq!(snapshot.window_start, start);
            assert_eq!(uuids(&snapshot), vec![2]);
            assert_eq!(snapshot.activity.turn_count(), 1);
            assert_eq!(snapshot.committed_bytes, start);
            let read = store.state.lock().unwrap().counters[1];
            assert!(read <= snapshot.stamp.size - start + 128 + base - start + 64 * 1024);
        }
    }

    #[test]
    fn windowed_alignment_preserves_source_budget_and_cancellation_at_a_yield() {
        let source = Source::new(&user("0").replace("hello", &"x".repeat(192 * 1024)));
        let owner = context("a");
        for cancel_next in [false, true] {
            let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_entry_bytes":256*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
            let mut request = windowed_acquire(&source.path, 1024);
            request["limits"].insert("max_source_read_bytes", json!(81));
            let origin = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(origin["status"].as_str(), Some("incomplete"));
            assert_eq!(origin["usage"]["source_bytes_read"].as_u64(), Some(64));
            let resume = |reply: &Value| json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":reply["cursor"]});
            let aligned = store.request(&resume(&origin), &owner, &Cancellation::default());
            assert_eq!(aligned["status"].as_str(), Some("incomplete"));
            assert_eq!(aligned["usage"]["source_bytes_read"].as_u64(), Some(17));
            let cancel = Cancellation::default();
            if cancel_next {
                cancel.cancel();
            }
            let stopped = store.request(&resume(&aligned), &owner, &cancel);
            assert_eq!(
                stopped["status"].as_str(),
                Some(if cancel_next {
                    "cancelled"
                } else {
                    "incomplete"
                }),
                "{stopped:?}"
            );
            assert_eq!(
                stopped["reason"].as_str(),
                Some(if cancel_next {
                    "request cancelled"
                } else {
                    "source_read_limit"
                })
            );
            assert_eq!(stopped["usage"]["source_bytes_read"].as_u64(), Some(0));
            assert!(stopped["cursor"].is_null());
            assert_eq!(store.state.lock().unwrap().counters[1], 81);
        }
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
        let before = store.state.lock().unwrap().counters;
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
            assert_eq!(state.counters[1], before[1]);
            assert_eq!(state.counters[7] - before[7], 2);
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
    fn acquire_describes_the_latest_non_synthetic_model() {
        let reply = |uuid: &str, model: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"{uuid}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"{model}","content":[{{"type":"text","text":"ok"}}]}}}}"#
            )
        };
        let source = Source::new(&format!(
            "{}{}\n{}\n{}\n{}",
            users(0..2),
            reply("older", "claude-opus-4-7"),
            reply("newer", "claude-opus-5-5"),
            reply("error", SYNTHETIC_MODEL),
            users(2..3)
        ));
        let silent = Source::new(&users(0..3));
        let store = windowed_store();
        let owner = context("a");
        let model = |path: &Path| {
            finish(
                &store,
                store.request(&acquire(path), &owner, &Cancellation::default()),
                &owner,
            )["data"]["description"]["model"]
                .clone()
        };

        assert_eq!(model(&source.path), json!("claude-opus-5-5"));
        assert_eq!(model(&silent.path), json!(null));
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
        assert!(unwindowed["data"]["description"]["window_started_unix_ms"].is_null());
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
        assert_eq!(cold["status"].as_str(), Some("ok"), "{cold:?}");
        assert_eq!(
            cold["usage"]["discovery_entries_examined"].as_u64(),
            Some(930)
        );
        assert_eq!(cold["usage"]["source_opens"].as_u64(), Some(0));
        let warm = store.request(&json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":prepare["thread_ids"],"roots":prepare["roots"],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &background, &Cancellation::default());
        assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
        assert_eq!(warm["data"]["complete"].as_bool(), Some(false));
        assert_eq!(
            warm["usage"]["discovery_entries_examined"].as_u64(),
            Some(0)
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
    fn restarted_owner_reuses_prepared_facts_for_unchanged_sources() {
        let namespace = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "cc-transcript-prepared-reuse-{}-{}",
                std::process::id(),
                now_ms()
            ));
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = Some(namespace.clone()));
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/changed"}}]}}"#;
        let source = Source::new(&format!("{}\n", user("root")));
        let changed = source.directory.join("changed.jsonl");
        let stable = source.directory.join("stable.jsonl");
        std::fs::write(&changed, format!("{}\n", user("before"))).unwrap();
        std::fs::write(&stable, format!("{}\n", user("stable"))).unwrap();
        let config = json!({"prepared_fact_producer":"restart-test"});
        let owner = context("a");
        let serve = |store: &NativeStore| {
            let root = finish(
                store,
                store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                &owner,
            );
            let template = acquire(&source.path);
            let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[stable.to_string_lossy().as_ref(),changed.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            let graph = finish(
                store,
                store.request(&prepare, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
            let answer = finish_prepared(
                store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Read","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(answer["status"].as_str(), Some("ok"), "{answer:?}");
            answer["data"]["value"].as_bool()
        };
        let first = NativeStore::new(&config).unwrap();
        assert_eq!(serve(&first), Some(false));
        let built = first.prepared_disk.stats().writes;
        drop(first);
        std::fs::write(&changed, format!("{}\n{tool}\n", user("after"))).unwrap();
        let restarted = NativeStore::new(&config).unwrap();
        assert_eq!(serve(&restarted), Some(true));
        let rebuilt = restarted.prepared_disk.stats().writes;
        assert!(
            rebuilt >= 1 && rebuilt < built,
            "the restarted owner wrote {rebuilt} facts where the first wrote {built}"
        );
        restarted.assert_conserved();
        drop(restarted);
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = None);
        std::fs::remove_dir_all(namespace).unwrap();
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
        assert_eq!(store.lock_state().remove_identity_facts(identity), 1);
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
        assert!(store.state.lock().unwrap().has_prepared_facts(&identity));
    }

    fn fact_entry_key(
        path: &Path,
        owner: &Value,
    ) -> crate::snapshot_prepared_disk::PreparedDiskKey {
        crate::snapshot_prepared_disk::PreparedDiskKey::new(
            SourceStamp::of(&std::fs::metadata(path).unwrap()),
            owner["registry_generation"].as_str().unwrap(),
            owner["admission"].as_str().unwrap(),
            &owner["authority"],
            &json!({"id":"native","version":"1"}),
        )
        .unwrap()
    }

    fn index_read_bytes(
        store: &NativeStore,
        key: &crate::snapshot_prepared_disk::PreparedDiskKey,
    ) -> u64 {
        (crate::snapshot_prepared_disk::HEADER_BYTES + store.prepared_disk.entry_index_bytes(key))
            as u64
    }

    fn evict_memory_facts(store: &NativeStore, path: &Path) {
        assert_eq!(
            store
                .lock_state()
                .remove_identity_facts(SourceStamp::of(&std::fs::metadata(path).unwrap()).identity),
            1
        );
    }

    fn cache_reads(reply: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
        (
            reply["usage"]["prepared_cache_reads"].as_u64(),
            reply["usage"]["prepared_cache_bytes_read"].as_u64(),
            reply["usage"]["source_bytes_read"].as_u64(),
        )
    }

    const ENTRY_LIMIT: (Option<&str>, Option<&str>) = (
        Some("source_limit"),
        Some("prepared facts entry exceeds the warming read budget"),
    );
    const ENTRY_EVICTED: (Option<&str>, Option<&str>) = (
        Some("incomplete"),
        Some("prepared facts revision was evicted"),
    );
    const DIRECTORY_CHANGED: (Option<&str>, Option<&str>) = (
        Some("incomplete"),
        Some("prepared facts cache directory changed"),
    );

    fn refusal(reply: &Value) -> (Option<&str>, Option<&str>) {
        (reply["status"].as_str(), reply["reason"].as_str())
    }

    #[test]
    fn disk_fact_hits_report_prepared_cache_reads_apart_from_source_reads() {
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
        let ask = || {
            finish_prepared(
                &store,
                store.request(&query, &owner, &Cancellation::default()),
                &owner,
            )
        };
        assert_eq!(ask()["data"]["value"].as_bool(), Some(false));
        let key = fact_entry_key(&sidechain, &owner);
        let stored = store.prepared_disk.entry_file_len(&key);
        let indexed = index_read_bytes(&store, &key);
        assert_eq!(indexed, stored);
        let from_memory = ask();
        assert_eq!(
            from_memory["data"]["value"].as_bool(),
            Some(false),
            "{from_memory:?}"
        );
        assert_eq!(cache_reads(&from_memory), (Some(0), Some(0), Some(0)));
        evict_memory_facts(&store, &sidechain);
        let before = store.state.lock().unwrap().counters;
        let from_disk = ask();
        assert_eq!(
            from_disk["data"]["value"].as_bool(),
            Some(false),
            "{from_disk:?}"
        );
        assert_eq!(cache_reads(&from_disk), (Some(1), Some(indexed), Some(0)));
        assert_eq!(from_disk["usage"]["cold_parses"].as_u64(), Some(0));
        let after = store.state.lock().unwrap().counters;
        assert_eq!(
            (
                after[18] - before[18],
                after[19] - before[19],
                after[1] - before[1]
            ),
            (1, indexed, 0)
        );
        evict_memory_facts(&store, &sidechain);
        let entry = store.prepared_disk.entry_file(&key);
        let mut bytes = std::fs::read(&entry).unwrap();
        bytes[crate::snapshot_prepared_disk::HEADER_BYTES + 8] ^= 1;
        std::fs::write(&entry, bytes).unwrap();
        let corrupt = ask();
        assert_eq!(
            corrupt["status"].as_str(),
            Some("incomplete"),
            "{corrupt:?}"
        );
        assert_eq!(
            corrupt["reason"].as_str(),
            Some("prepared facts revision was evicted")
        );
        assert_eq!(cache_reads(&corrupt), (Some(1), Some(indexed), Some(0)));
        assert!(!store.lock_state().has_prepared_facts(
            &SourceStamp::of(&std::fs::metadata(&sidechain).unwrap()).identity
        ));
        let stats = store.request(
            &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
            &owner,
            &Cancellation::default(),
        );
        assert_eq!(
            (
                stats["data"]["counters"]["prepared_cache_reads"].as_u64(),
                stats["data"]["counters"]["prepared_cache_bytes_read"].as_u64(),
            ),
            (Some(after[18] + 1), Some(after[19] + indexed))
        );
    }

    #[test]
    fn prepared_query_resolves_each_directory_once_per_pass() {
        let source = Source::new(&format!("{}\n", user("root")));
        let children: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|name| {
                let child = source.directory.join(format!("{name}.jsonl"));
                std::fs::write(&child, format!("{}\n", user(name))).unwrap();
                child
            })
            .collect();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let direct_paths: Vec<_> = children
            .iter()
            .map(|child| child.to_string_lossy().into_owned())
            .collect();
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":direct_paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Missing","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let traversal = || {
            let before = (
                store.source_resolutions.load(Ordering::Relaxed),
                store.fact_lookups.load(Ordering::Relaxed),
            );
            let reply = finish_prepared(
                &store,
                store.request(&query, &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(reply["data"]["value"].as_bool(), Some(false), "{reply:?}");
            (
                store.source_resolutions.load(Ordering::Relaxed) - before.0,
                store.fact_lookups.load(Ordering::Relaxed) - before.1,
                cache_reads(&reply).0,
            )
        };
        traversal();
        assert_eq!(traversal(), (4, 0, Some(0)));
        for child in &children {
            evict_memory_facts(&store, child);
        }
        assert_eq!(traversal(), (4, 3, Some(3)));
    }

    fn retarget_on_resolve(store: &NativeStore, trigger: &Path, directory: &Path, moved: &Path) {
        let armed = Arc::new(AtomicBool::new(true));
        let (trigger, directory, moved) = (
            trigger.to_path_buf(),
            directory.to_path_buf(),
            moved.to_path_buf(),
        );
        *store.resolve_hook.lock().unwrap() = Some(Arc::new(move |path: &Path| {
            if path == trigger && armed.swap(false, Ordering::SeqCst) {
                std::fs::rename(&directory, &moved).unwrap();
                std::os::unix::fs::symlink(&moved, &directory).unwrap();
            }
        }));
    }

    fn canonical_members(directory: &Path, names: &[&str], extra: &str) -> Vec<PreparedSourceRef> {
        std::fs::create_dir_all(directory).unwrap();
        names
            .iter()
            .map(|name| {
                let path = directory.join(format!("{name}.jsonl"));
                std::fs::write(&path, format!("{}\n{extra}", user(name))).unwrap();
                let path = std::fs::canonicalize(&path).unwrap();
                PreparedSourceRef {
                    stamp: SourceStamp::of(&std::fs::metadata(&path).unwrap()),
                    path,
                }
            })
            .collect()
    }

    #[test]
    fn source_validation_rejects_a_parent_retargeted_between_two_members() {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = source.directory.join("members");
        let sources = canonical_members(&members, &["a", "b"], "");
        let store = store();
        let owner = context("a");
        let validate = || {
            store
                .validate_source_stamps(
                    &sources,
                    &owner,
                    &Cancellation::default(),
                    now_ms() + 30_000,
                )
                .unwrap()
        };
        assert!(validate());
        retarget_on_resolve(
            &store,
            &sources[0].path,
            &members,
            &source.directory.join("moved"),
        );
        assert!(!validate());
        assert!(sources.iter().all(|member| SourceStamp::of(
            &std::fs::metadata(&member.path).unwrap()
        ) == member.stamp));
    }

    #[test]
    fn restricted_roots_resolve_every_member_exactly() {
        let source = Source::new(&format!("{}\n", user("root")));
        let inside = source.directory.join("inside");
        let members = inside.join("d");
        let sources = canonical_members(&members, &["a", "b"], "");
        std::fs::create_dir(source.directory.join("outside")).unwrap();
        let store = store();
        let mut owner = context("a");
        owner.insert(
            "authority",
            json!({"kind":"restricted_roots","roots":[inside.to_string_lossy().as_ref()],"effective_uid":unsafe { libc::geteuid() }.to_string()}),
        );
        let validate = || {
            store.validate_source_stamps(
                &sources,
                &owner,
                &Cancellation::default(),
                now_ms() + 30_000,
            )
        };
        let before = store.source_resolutions.load(Ordering::Relaxed);
        assert!(validate().unwrap());
        assert_eq!(store.source_resolutions.load(Ordering::Relaxed) - before, 2);
        retarget_on_resolve(
            &store,
            &sources[0].path,
            &members,
            &source.directory.join("outside").join("d"),
        );
        assert!(validate().unwrap_err().status == Status::PermissionDenied);
    }

    #[test]
    fn source_validation_skips_its_parent_hold_once_cancelled_or_past_its_deadline() {
        let source = Source::new(&format!("{}\n", user("root")));
        let sources = canonical_members(&source.directory.join("members"), &["a", "b"], "");
        let store = store();
        let owner = context("a");
        let validate = |cancel: &Cancellation, deadline: u64| {
            let before = store.source_resolutions.load(Ordering::Relaxed);
            let validated = store
                .validate_source_stamps(&sources, &owner, cancel, deadline)
                .map_err(|error| error.status);
            (
                validated,
                store.source_resolutions.load(Ordering::Relaxed) - before,
            )
        };
        assert_eq!(
            validate(&Cancellation::default(), now_ms() + 30_000),
            (Ok(true), 2)
        );
        for (cancels, status) in [(true, Status::Cancelled), (false, Status::Deadline)] {
            let cancel = Cancellation::default();
            let deadline = now_ms() + 1_000;
            *store.resolve_hook.lock().unwrap() = Some(Arc::new({
                let (cancel, last) = (cancel.clone(), sources[1].path.clone());
                move |path: &Path| {
                    if path != last {
                        return;
                    }
                    if cancels {
                        cancel.cancel();
                    } else {
                        while now_ms() <= deadline {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                }
            }));
            assert_eq!(validate(&cancel, deadline), (Err(status), 1));
        }
    }

    #[test]
    fn empty_membership_validation_reports_cancellation_and_an_expired_deadline() {
        let store = store();
        let owner = context("a");
        let cancelled = Cancellation::default();
        cancelled.cancel();
        for (cancel, deadline, status) in [
            (Cancellation::default(), now_ms(), Status::Deadline),
            (cancelled, now_ms() + 30_000, Status::Cancelled),
        ] {
            let before = store.source_resolutions.load(Ordering::Relaxed);
            assert_eq!(
                (
                    store
                        .validate_source_stamps(&[], &owner, &cancel, deadline)
                        .map_err(|error| error.status),
                    store
                        .validate_sidechain_dirs(&[], &owner, &cancel, deadline)
                        .map_err(|error| error.status),
                ),
                (Err(status), Err(status))
            );
            assert_eq!(store.source_resolutions.load(Ordering::Relaxed), before);
        }
    }

    #[test]
    fn early_positive_query_rejects_a_parent_retargeted_during_its_page() {
        let source = Source::new(&format!("{}\n", user("root")));
        let tool = r#"{"type":"assistant","uuid":"tool","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"test","content":[{"type":"tool_use","id":"read","name":"Read","input":{"file_path":"/tmp/only-a"}}]}}"#;
        let members = source.directory.join("members");
        let sources = canonical_members(&members, &["a"], &format!("{tool}\n"));
        std::fs::write(members.join("b.jsonl"), format!("{}\n", user("b"))).unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let template = acquire(&source.path);
        let direct = [members.join("a.jsonl"), members.join("b.jsonl")]
            .map(|path| path.to_string_lossy().into_owned());
        let graph = finish_prepared(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":direct,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
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
        assert_eq!(query()["data"]["value"].as_bool(), Some(true));
        retarget_on_resolve(
            &store,
            &sources[0].path,
            &members,
            &source.directory.join("moved"),
        );
        let retargeted = query();
        assert_eq!(
            retargeted["status"].as_str(),
            Some("changed"),
            "{retargeted:?}"
        );
    }

    #[test]
    fn registered_warming_meters_prepared_cache_reads_against_its_read_budget() {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = [
            source.directory.join("thread-a.jsonl"),
            source.directory.join("thread-b.jsonl"),
        ];
        for (member, id) in members.iter().zip(["a", "b"]) {
            std::fs::write(member, format!("{}\n", user(id))).unwrap();
        }
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let send = |budget: u64, start: &Value, revision: &Value| {
            let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":["thread-a","thread-b"],"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":start,"membership_revision":revision,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            request["limits"].insert("max_source_read_bytes", json!(budget));
            store.request(&request, &owner, &Cancellation::default())
        };
        let warm = |budget: u64, start: &Value, revision: &Value| {
            let reply = send(budget, start, revision);
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            reply
        };
        let evict = || {
            for member in &members {
                evict_memory_facts(&store, member);
            }
        };
        let resident = || {
            let state = store.lock_state();
            members
                .iter()
                .filter(|member| {
                    state.has_prepared_facts(
                        &SourceStamp::of(&std::fs::metadata(member).unwrap()).identity,
                    )
                })
                .count()
        };
        let mut built = warm(1024 * 1024, &json!(0), &json!(null));
        for _ in 0..16 {
            if built["data"]["complete"].as_bool() == Some(true) {
                break;
            }
            let (next, revision) = (
                built["data"]["next_index"].clone(),
                built["data"]["membership_revision"].clone(),
            );
            built = warm(1024 * 1024, &next, &revision);
        }
        assert_eq!(built["data"]["complete"].as_bool(), Some(true), "{built:?}");
        assert_eq!(cache_reads(&built).0, Some(0));
        let stored = store
            .prepared_disk
            .entry_file_len(&fact_entry_key(&members[0], &owner));
        assert_eq!(
            store
                .prepared_disk
                .entry_file_len(&fact_entry_key(&members[1], &owner)),
            stored
        );
        let indexed = index_read_bytes(&store, &fact_entry_key(&members[0], &owner));
        assert_eq!(indexed, stored);

        evict();
        let both = warm(2 * indexed, &json!(0), &json!(null));
        assert_eq!(both["data"]["complete"].as_bool(), Some(true), "{both:?}");
        assert_eq!(cache_reads(&both), (Some(2), Some(2 * indexed), Some(0)));
        assert_eq!(resident(), 2);

        evict();
        let first = warm(2 * indexed - 1, &json!(0), &json!(null));
        assert_eq!(
            first["data"]["complete"].as_bool(),
            Some(false),
            "{first:?}"
        );
        assert_eq!(first["data"]["next_index"].as_u64(), Some(1));
        assert_eq!(cache_reads(&first), (Some(1), Some(indexed), Some(0)));
        let second = warm(
            2 * indexed - 1,
            &first["data"]["next_index"],
            &first["data"]["membership_revision"],
        );
        assert_eq!(
            second["data"]["complete"].as_bool(),
            Some(true),
            "{second:?}"
        );
        assert_eq!(cache_reads(&second), (Some(1), Some(indexed), Some(0)));
        assert_eq!(resident(), 2);

        evict();
        for _ in 0..2 {
            let oversize = send(indexed - 1, &json!(0), &json!(null));
            assert_eq!(refusal(&oversize), ENTRY_LIMIT, "{oversize:?}");
            assert_eq!(cache_reads(&oversize), (Some(0), Some(0), Some(0)));
        }
        assert_eq!(resident(), 0);
        assert_eq!(store.prepared_disk.stats().writes, 2);
    }

    fn fact_budget_store(fact_budget: usize) -> NativeStore {
        NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":fact_budget,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
    }

    fn background_owner() -> Value {
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        owner
    }

    fn registered_members(source: &Source, ids: &[&str]) -> Vec<PathBuf> {
        ids.iter()
            .map(|id| {
                let member = source.directory.join(format!("{id}.jsonl"));
                std::fs::write(&member, format!("{}\n", user(id))).unwrap();
                member
            })
            .collect()
    }

    fn member_identity(member: &Path) -> SourceIdentity {
        SourceStamp::of(&std::fs::metadata(member).unwrap()).identity
    }

    fn resident_member_facts(store: &NativeStore, members: &[PathBuf]) -> Vec<Option<usize>> {
        let state = store.lock_state();
        members
            .iter()
            .map(|member| {
                state
                    .identity_facts(member_identity(member))
                    .next()
                    .map(|(_, cached)| cached.facts.accounted_bytes())
            })
            .collect()
    }

    fn resident_member_is_full(store: &NativeStore, member: &Path) -> Option<bool> {
        let state = store.lock_state();
        let full = state
            .identity_facts(member_identity(member))
            .next()
            .map(|(_, cached)| cached.facts.is_full());
        full
    }

    fn forget_member_facts(store: &NativeStore, members: &[PathBuf]) {
        let mut state = store.lock_state();
        for member in members {
            state.remove_identity_facts(member_identity(member));
        }
    }

    fn age_member_facts(store: &NativeStore, member: &Path) {
        let mut state = store.lock_state();
        let (key, previous) = {
            let (key, cached) = state
                .identity_facts(member_identity(member))
                .next()
                .unwrap();
            (*key, cached.last_used)
        };
        state.prepared_facts_lru.remove(&(previous, key));
        state.prepared_facts_lru.insert((0, key));
        state.prepared_facts.get_mut(&key).unwrap().last_used = 0;
    }

    fn warm_registered_pass(
        store: &NativeStore,
        source: &Source,
        ids: &[&str],
        owner: &Value,
    ) -> (u64, u64, u64) {
        let template = acquire(&source.path);
        let (mut start, mut revision, mut reads) = (json!(0), json!(null), (0, 0, 0));
        for _ in 0..16 {
            let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":start,"membership_revision":revision,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            request["limits"].insert("max_source_read_bytes", json!(1024 * 1024));
            let reply = store.request(&request, owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            let (count, bytes, source_bytes) = cache_reads(&reply);
            reads = (
                reads.0 + count.unwrap(),
                reads.1 + bytes.unwrap(),
                reads.2 + source_bytes.unwrap(),
            );
            if reply["data"]["complete"].as_bool() == Some(true) {
                return reads;
            }
            (start, revision) = (
                reply["data"]["next_index"].clone(),
                reply["data"]["membership_revision"].clone(),
            );
        }
        panic!("registered warming never completed");
    }

    fn registered_fact_sizes(
        source: &Source,
        members: &[PathBuf],
        ids: &[&str],
        owner: &Value,
    ) -> (usize, usize) {
        let probe = fact_budget_store(16 * 1024 * 1024);
        warm_registered_pass(&probe, source, ids, owner);
        let built = resident_member_facts(&probe, members);
        forget_member_facts(&probe, members);
        assert_eq!(
            warm_registered_pass(&probe, source, ids, owner).0,
            members.len() as u64
        );
        let decoded = resident_member_facts(&probe, members);
        assert!(built.iter().all(|bytes| *bytes == built[0]), "{built:?}");
        assert!(
            decoded.iter().all(|bytes| *bytes == decoded[0]),
            "{decoded:?}"
        );
        (built[0].unwrap(), decoded[0].unwrap())
    }

    #[test]
    fn registered_warming_leaves_disk_facts_unread_when_keeping_them_would_evict_its_own_members() {
        let source = Source::new(&format!("{}\n", user("root")));
        let ids = ["thread-a", "thread-b", "thread-c"];
        let members = registered_members(&source, &ids);
        let owner = background_owner();
        let (built, _) = registered_fact_sizes(&source, &members, &ids, &owner);
        let store = fact_budget_store(2 * built + built / 2);
        let resident = || {
            resident_member_facts(&store, &members)
                .iter()
                .flatten()
                .count()
        };
        assert!(warm_registered_pass(&store, &source, &ids, &owner).2 > 0);
        assert_eq!(resident(), 2);
        for _ in 0..3 {
            assert_eq!(
                warm_registered_pass(&store, &source, &ids, &owner),
                (0, 0, 0)
            );
            assert_eq!(resident(), 2);
        }
        assert!(members.iter().all(|member| store
            .prepared_disk
            .has_entry(&fact_entry_key(member, &owner))
            .unwrap()));
        assert_eq!(store.prepared_disk.stats().writes, 3);
    }

    #[test]
    fn registered_warming_rereads_disk_facts_when_its_membership_fits_the_fact_budget() {
        let source = Source::new(&format!("{}\n", user("root")));
        let ids = ["thread-a", "thread-b", "thread-c"];
        let members = registered_members(&source, &ids);
        let owner = background_owner();
        let (built, decoded) = registered_fact_sizes(&source, &members, &ids, &owner);
        let store = fact_budget_store(3 * built + built / 2);
        assert!(warm_registered_pass(&store, &source, &ids, &owner).2 > 0);
        for member in &members {
            evict_memory_facts(&store, member);
        }
        let stored = store
            .prepared_disk
            .entry_file_len(&fact_entry_key(&members[0], &owner));
        assert!(members.iter().all(|member| store
            .prepared_disk
            .entry_file_len(&fact_entry_key(member, &owner))
            == stored));
        let indexed = index_read_bytes(&store, &fact_entry_key(&members[0], &owner));
        assert_eq!(
            warm_registered_pass(&store, &source, &ids, &owner),
            (3, 3 * indexed, 0)
        );
        assert_eq!(resident_member_facts(&store, &members), [Some(decoded); 3]);
    }

    #[test]
    fn registered_warming_still_evicts_older_facts_outside_its_membership() {
        let source = Source::new(&format!("{}\n", user("root")));
        let ids = ["thread-a", "thread-b", "thread-x"];
        let paths = registered_members(&source, &ids);
        let (members, outsider) = paths.split_at(2);
        let owner = background_owner();
        let (built, decoded) = registered_fact_sizes(&source, &paths, &ids, &owner);
        let store = fact_budget_store(built + decoded + decoded / 2);
        assert!(warm_registered_pass(&store, &source, &ids[..2], &owner).2 > 0);
        forget_member_facts(&store, members);
        assert!(warm_registered_pass(&store, &source, &ids[2..], &owner).2 > 0);
        assert_eq!(resident_member_facts(&store, outsider), [Some(built)]);
        age_member_facts(&store, &outsider[0]);
        let indexed = index_read_bytes(&store, &fact_entry_key(&members[0], &owner));
        assert_eq!(
            warm_registered_pass(&store, &source, &ids[..2], &owner),
            (2, 2 * indexed, 0)
        );
        assert_eq!(resident_member_facts(&store, members), [Some(decoded); 2]);
        assert_eq!(resident_member_facts(&store, outsider), [None]);
    }

    #[test]
    fn registered_warming_charges_its_member_positions_once_and_reuses_them_on_every_page() {
        let (source, store, owner, fresh) = registered_warming(20);
        let key = NativeStore::warm_membership_key(&fresh, &owner).unwrap();
        let positions = || {
            let state = store.lock_state();
            assert_eq!(state.warm_memberships.len(), 1);
            let membership = &state.warm_memberships[&key];
            assert!(membership.members.iter().all(|member| membership
                .active_member(member.stamp.identity, None)
                .is_some_and(|found| std::ptr::eq(found, member))));
            assert_eq!(
                state.ledger.shared.warm(),
                source_ref_bytes(&membership.members)
                    + arc_slice_mirror::<(SourceIdentity, usize)>(membership.members.len())
                    + sidechain_dir_bytes(&membership.sidechain_dirs)
            );
            slice_key(&membership.positions)
        };
        let mut request = fresh.clone();
        let mut pages = Vec::new();
        for _ in 0..8 {
            request.insert("deadline_unix_ms", json!(now_ms() + 120_000));
            let reply = store.request(&request, &owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            pages.push((reply["data"]["next_index"].as_u64().unwrap(), positions()));
            store.assert_conserved();
            if reply["data"]["complete"].as_bool() == Some(true) {
                break;
            }
            request.insert("start_index", reply["data"]["next_index"].clone());
            request.insert(
                "membership_revision",
                reply["data"]["membership_revision"].clone(),
            );
        }
        let built = pages[0].1;
        assert_eq!(pages, [(8, built), (16, built), (20, built)]);
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);

        let cancelled = Cancellation::default();
        cancelled.cancel();
        let rejected = store.request(&request, &owner, &cancelled);
        assert_eq!(
            rejected["status"].as_str(),
            Some("cancelled"),
            "{rejected:?}"
        );
        assert_eq!(positions(), built);
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);

        std::fs::OpenOptions::new()
            .append(true)
            .open(source.directory.join("thread-0007.jsonl"))
            .unwrap()
            .write_all(format!("{}\n", user("appended")).as_bytes())
            .unwrap();
        registered_pages(&store, &owner, &fresh);
        positions();
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        store.assert_conserved();

        store.lock_state().remove_warm_membership(&key);
        assert_eq!(store.lock_state().ledger.shared.warm(), 0);
        store.assert_conserved();
    }

    #[test]
    fn registered_warming_reads_inherited_disk_facts_its_fact_budget_cannot_keep() {
        let namespace = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "cc-transcript-warm-inherited-{}-{}",
                std::process::id(),
                now_ms()
            ));
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = Some(namespace.clone()));
        let source = Source::new(&format!("{}\n", user("root")));
        let ids = ["thread-a", "thread-b", "thread-c"];
        let members = registered_members(&source, &ids);
        let owner = background_owner();
        let (built, decoded) = registered_fact_sizes(&source, &members, &ids, &owner);
        let producing = |fact_budget: usize| {
            NativeStore::new(&json!({"prepared_fact_producer":"warm-inherited-test","max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":fact_budget,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
        };
        let first = producing(16 * 1024 * 1024);
        assert!(warm_registered_pass(&first, &source, &ids, &owner).2 > 0);
        let indexed = index_read_bytes(&first, &fact_entry_key(&members[0], &owner));
        drop(first);
        let restarted = producing(built + decoded + decoded / 2);
        assert_eq!(
            warm_registered_pass(&restarted, &source, &ids, &owner),
            (3, 3 * indexed, 0)
        );
        assert_eq!(
            warm_registered_pass(&restarted, &source, &ids, &owner),
            (0, 0, 0)
        );
        restarted.assert_conserved();
        drop(restarted);
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = None);
        std::fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn metered_warming_stays_within_its_budget_when_an_entry_grows_under_the_read() {
        let source = Source::new(&format!("{}\n", user("root")));
        let member = source.directory.join("thread-a.jsonl");
        std::fs::write(&member, format!("{}\n", user("a"))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let warm = |budget: u64| {
            let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":["thread-a"],"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            request["limits"].insert("max_source_read_bytes", json!(budget));
            store.request(&request, &owner, &Cancellation::default())
        };
        let mut built = warm(1024 * 1024);
        for _ in 0..16 {
            if built["data"]["complete"].as_bool() == Some(true) {
                break;
            }
            built = warm(1024 * 1024);
        }
        assert_eq!(built["data"]["complete"].as_bool(), Some(true), "{built:?}");
        let key = fact_entry_key(&member, &owner);
        let indexed = index_read_bytes(&store, &key);
        let entry = store.prepared_disk.entry_file(&key);
        let identity = SourceStamp::of(&std::fs::metadata(&member).unwrap()).identity;
        let grown = Arc::new(AtomicUsize::new(0));
        let growths = Arc::clone(&grown);
        store.prepared_disk.set_read_hook(Some(Box::new(move || {
            growths.fetch_add(1, Ordering::Relaxed);
            std::fs::OpenOptions::new()
                .append(true)
                .open(&entry)
                .unwrap()
                .write_all(b"grown")
                .unwrap();
        })));
        evict_memory_facts(&store, &member);
        let refused = warm(indexed - 1);
        let admitted = warm(indexed);
        store.prepared_disk.set_read_hook(None);
        assert_eq!(refusal(&refused), ENTRY_LIMIT, "{refused:?}");
        assert_eq!(cache_reads(&refused), (Some(0), Some(0), Some(0)));
        assert_eq!(refusal(&admitted), ENTRY_EVICTED, "{admitted:?}");
        assert_eq!(cache_reads(&admitted), (Some(1), Some(indexed), Some(0)));
        assert_eq!(grown.load(Ordering::Relaxed), 1);
        assert!(!store.lock_state().has_prepared_facts(&identity));
        assert!(!store.prepared_disk.has_entry(&key).unwrap());
        let rebuilt = warm(1024 * 1024);
        assert_eq!(rebuilt["status"].as_str(), Some("ok"), "{rebuilt:?}");
        assert_eq!(rebuilt["data"]["complete"].as_bool(), Some(true));
        assert_eq!(rebuilt["data"]["fact_cache_writes"].as_u64(), Some(2));
        assert!(store.lock_state().has_prepared_facts(&identity));
    }

    struct FactReadGate {
        entered: std::sync::mpsc::Receiver<()>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl FactReadGate {
        fn wait_until_read(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(10))
                .expect("prepared facts read hook was not reached within ten seconds");
        }

        fn release(&self) {
            let (released, notification) = &*self.released;
            *released.lock().unwrap() = true;
            notification.notify_all();
        }
    }

    fn install_fact_read_gate(
        store: &NativeStore,
        mut before_release: impl FnMut() + Send + 'static,
    ) -> FactReadGate {
        let (sender, entered) = std::sync::mpsc::channel();
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let gated = Arc::clone(&released);
        let mut entries = 0usize;
        store.prepared_disk.set_read_hook(Some(Box::new(move || {
            entries += 1;
            if entries > 1 {
                return;
            }
            sender.send(()).expect("fact read gate owner disappeared");
            let (released, notification) = &*gated;
            let (released, _) = notification
                .wait_timeout_while(
                    released.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            assert!(
                *released,
                "fact read gate was not released within ten seconds"
            );
            drop(released);
            before_release();
        })));
        FactReadGate { entered, released }
    }

    fn wait_for_fact_read_waiter(store: &NativeStore) {
        let started = std::time::Instant::now();
        while store.fact_read_waits.load(Ordering::Relaxed) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the second reader never waited on the fact read permit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn warm_request(source: &Source, ids: &[&str], deadline_unix_ms: u64) -> Value {
        let template = acquire(&source.path);
        let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":deadline_unix_ms,"limits":template["limits"]});
        request["limits"].insert("max_source_read_bytes", json!(1024 * 1024));
        request
    }

    fn spawn_warm(
        store: &Arc<NativeStore>,
        request: Value,
        owner: &Value,
        cancel: Cancellation,
    ) -> std::sync::mpsc::Receiver<Value> {
        let (sender, reply) = std::sync::mpsc::channel();
        let store = Arc::clone(store);
        let owner = owner.clone();
        std::thread::spawn(move || {
            sender
                .send(store.request(&request, &owner, &cancel))
                .expect("warm reply receiver disappeared");
        });
        reply
    }

    fn warm_reply(reply: &std::sync::mpsc::Receiver<Value>) -> Value {
        reply
            .recv_timeout(Duration::from_secs(10))
            .expect("warm request did not finish within ten seconds")
    }

    struct EvictedMembers {
        source: Source,
        members: Vec<PathBuf>,
        store: Arc<NativeStore>,
        owner: Value,
        indexed: u64,
        built: Vec<Option<usize>>,
    }

    fn evicted_members(ids: &[&str]) -> EvictedMembers {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = registered_members(&source, ids);
        let store = Arc::new(fact_budget_store(16 * 1024 * 1024));
        let owner = background_owner();
        warm_registered_pass(&store, &source, ids, &owner);
        let indexed = index_read_bytes(&store, &fact_entry_key(&members[0], &owner));
        let built = resident_member_facts(&store, &members);
        assert!(built.iter().all(Option::is_some), "{built:?}");
        forget_member_facts(&store, &members);
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            built,
        }
    }

    #[test]
    fn concurrent_identical_fact_misses_share_one_disk_read_and_one_publication() {
        let EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            ..
        } = evicted_members(&["thread-a"]);
        let gate = install_fact_read_gate(&store, || {});
        let deadline = now_ms() + 30_000;
        let first = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let second = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        wait_for_fact_read_waiter(&store);
        gate.release();
        let (first, second) = (warm_reply(&first), warm_reply(&second));
        store.prepared_disk.set_read_hook(None);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert_eq!(first["data"]["complete"].as_bool(), Some(true));
        assert_eq!(second["data"]["complete"].as_bool(), Some(true));
        assert_eq!(cache_reads(&first), (Some(1), Some(indexed), Some(0)));
        assert_eq!(cache_reads(&second), (Some(0), Some(0), Some(0)));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert_eq!(store.fact_read_waits.load(Ordering::Relaxed), 1);
        assert!(resident_member_facts(&store, &members)
            .iter()
            .all(Option::is_some));
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
    }

    #[test]
    fn waiting_fact_reader_honors_cancellation_while_the_holder_is_blocked() {
        let EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            ..
        } = evicted_members(&["thread-a"]);
        let gate = install_fact_read_gate(&store, || {});
        let deadline = now_ms() + 30_000;
        let first = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let cancellation = Cancellation::default();
        let second = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            cancellation.clone(),
        );
        wait_for_fact_read_waiter(&store);
        cancellation.cancel();
        let cancelled = warm_reply(&second);
        assert_eq!(
            cancelled["status"].as_str(),
            Some("cancelled"),
            "{cancelled:?}"
        );
        assert!(store.lock_state().fact_read_in_flight);
        gate.release();
        let first = warm_reply(&first);
        store.prepared_disk.set_read_hook(None);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(cache_reads(&first), (Some(1), Some(indexed), Some(0)));
        assert_eq!(cache_reads(&cancelled), (Some(0), Some(0), Some(0)));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert!(resident_member_facts(&store, &members)
            .iter()
            .all(Option::is_some));
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
    }

    #[test]
    fn waiting_fact_reader_honors_its_deadline_while_the_holder_is_blocked() {
        let EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            ..
        } = evicted_members(&["thread-a"]);
        let gate = install_fact_read_gate(&store, || {});
        let first = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], now_ms() + 30_000),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let second = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], now_ms() + 1_500),
            &owner,
            Cancellation::default(),
        );
        wait_for_fact_read_waiter(&store);
        let expired = warm_reply(&second);
        assert_eq!(expired["status"].as_str(), Some("deadline"), "{expired:?}");
        assert!(store.lock_state().fact_read_in_flight);
        gate.release();
        let first = warm_reply(&first);
        store.prepared_disk.set_read_hook(None);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(cache_reads(&first), (Some(1), Some(indexed), Some(0)));
        assert_eq!(cache_reads(&expired), (Some(0), Some(0), Some(0)));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert!(resident_member_facts(&store, &members)
            .iter()
            .all(Option::is_some));
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
    }

    #[test]
    fn failed_fact_read_releases_the_permit_for_the_next_caller() {
        let EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            built,
        } = evicted_members(&["thread-a"]);
        let key = fact_entry_key(&members[0], &owner);
        let entry = store.prepared_disk.entry_file(&key);
        let gate = install_fact_read_gate(&store, move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&entry)
                .unwrap()
                .write_all(b"grown")
                .unwrap();
        });
        let deadline = now_ms() + 30_000;
        let first = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let second = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        wait_for_fact_read_waiter(&store);
        gate.release();
        let (evicted, rebuilt) = (warm_reply(&first), warm_reply(&second));
        store.prepared_disk.set_read_hook(None);
        assert_eq!(refusal(&evicted), ENTRY_EVICTED, "{evicted:?}");
        assert_eq!(cache_reads(&evicted), (Some(1), Some(indexed), Some(0)));
        assert_eq!(rebuilt["status"].as_str(), Some("ok"), "{rebuilt:?}");
        assert_eq!(rebuilt["data"]["complete"].as_bool(), Some(true));
        assert_eq!(rebuilt["data"]["fact_cache_writes"].as_u64(), Some(2));
        assert_eq!(cache_reads(&rebuilt).0, Some(0));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert!(store.prepared_disk.has_entry(&key).unwrap());
        assert_eq!(resident_member_facts(&store, &members), built);
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
    }

    #[test]
    fn different_fact_keys_do_not_share_facts_under_the_permit() {
        let EvictedMembers {
            source,
            members,
            store,
            owner,
            indexed,
            ..
        } = evicted_members(&["thread-a", "thread-b"]);
        let gate = install_fact_read_gate(&store, || {});
        let deadline = now_ms() + 30_000;
        let first = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let second = spawn_warm(
            &store,
            warm_request(&source, &["thread-b"], deadline),
            &owner,
            Cancellation::default(),
        );
        wait_for_fact_read_waiter(&store);
        gate.release();
        let (first, second) = (warm_reply(&first), warm_reply(&second));
        store.prepared_disk.set_read_hook(None);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert_eq!(cache_reads(&first), (Some(1), Some(indexed), Some(0)));
        assert_eq!(cache_reads(&second).0, Some(1));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 2);
        assert!(resident_member_facts(&store, &members)
            .iter()
            .all(Option::is_some));
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
    }

    #[test]
    fn a_stopped_fact_read_keeps_the_entry_for_the_next_member_reader() {
        for (expected, cancels) in [("cancelled", true), ("deadline", false)] {
            let EvictedMembers {
                source,
                members,
                store,
                owner,
                indexed,
                ..
            } = evicted_members(&["thread-a"]);
            let key = fact_entry_key(&members[0], &owner);
            let cancellation = Cancellation::default();
            let stop = cancellation.clone();
            let deadline = now_ms() + if cancels { 30_000 } else { 1_500 };
            store.prepared_disk.set_read_hook(Some(Box::new(move || {
                if cancels {
                    stop.cancel();
                } else {
                    while now_ms() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })));
            let stopped = store.request(
                &warm_request(&source, &["thread-a"], deadline),
                &owner,
                &cancellation,
            );
            store.prepared_disk.set_read_hook(None);
            assert_eq!(stopped["status"].as_str(), Some(expected), "{stopped:?}");
            assert_eq!(cache_reads(&stopped), (Some(0), Some(0), Some(0)));
            assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
            assert!(store.prepared_disk.has_entry(&key).unwrap());
            assert!(!store.lock_state().fact_read_in_flight);
            store.assert_conserved();
            let reused = store.request(
                &warm_request(&source, &["thread-a"], now_ms() + 30_000),
                &owner,
                &Cancellation::default(),
            );
            assert_eq!(reused["status"].as_str(), Some("ok"), "{reused:?}");
            assert_eq!(reused["data"]["complete"].as_bool(), Some(true));
            assert_eq!(cache_reads(&reused), (Some(1), Some(indexed), Some(0)));
            assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 2);
            assert!(resident_member_facts(&store, &members)
                .iter()
                .all(Option::is_some));
            store.assert_conserved();
        }
    }

    fn user_text(id: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{id}","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{{"content":"{text}"}}}}"#
        )
    }

    fn tool_use_event(id: &str, name: &str, path: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{id}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{"file_path":"{path}"}}}}]}}}}"#
        )
    }

    fn tool_result_event(id: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{id}-result","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":"{content}"}}]}}}}"#
        )
    }

    fn compact_facts_store() -> NativeStore {
        NativeStore::new(&json!({"max_read_bytes_per_step":2*1024*1024,"max_entry_bytes":4*1024*1024,"max_retained_bytes":64*1024*1024,"max_prepared_fact_memory_bytes":16*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
    }

    fn wide_template(source: &Source) -> Value {
        let mut template = acquire(&source.path);
        template["limits"].insert("max_read_bytes", json!(8 * 1024 * 1024));
        template["limits"].insert("max_source_read_bytes", json!(8 * 1024 * 1024));
        template
    }

    fn has_tool(pattern: &str) -> Value {
        json!({"kind":"has_tool","pattern":pattern,"subagents":true})
    }

    fn has_override(token: &str, invalidated_by: &[&str]) -> Value {
        json!({"kind":"has_override","token":token,"invalidated_by":invalidated_by,"subagents":true})
    }

    fn value_of(reply: &Value) -> Option<bool> {
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        reply["data"]["value"].as_bool()
    }

    fn single_step(reply: Value) -> Value {
        assert!(reply["cursor"].is_null(), "{reply:?}");
        reply
    }

    fn prepared_graph_request(
        graph: &Value,
        template: &Value,
        selectors: Value,
        query: Value,
        observers: Option<Value>,
    ) -> Value {
        let mut request = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":selectors,"query":query,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        if let Some(observers) = observers {
            request.insert("observers", observers);
        }
        request
    }

    struct CompactGraph {
        source: Source,
        member: PathBuf,
        store: NativeStore,
        owner: Value,
        template: Value,
        graph: Value,
    }

    impl CompactGraph {
        fn ask(&self, selectors: Value, query: Value, observers: Option<Value>) -> Value {
            single_step(self.store.request(
                &prepared_graph_request(&self.graph, &self.template, selectors, query, observers),
                &self.owner,
                &Cancellation::default(),
            ))
        }

        fn member_key(&self) -> crate::snapshot_prepared_disk::PreparedDiskKey {
            fact_entry_key(&self.member, &self.owner)
        }

        fn root_key(&self) -> crate::snapshot_prepared_disk::PreparedDiskKey {
            fact_entry_key(&self.source.path, &self.owner)
        }
    }

    fn bulk_member_graph() -> CompactGraph {
        bulk_member_graph_with(compact_facts_store())
    }

    fn bulk_member_graph_with(store: NativeStore) -> CompactGraph {
        let source = Source::new(&format!("{}\n", user_text("root", "root-prompt")));
        let member = source.directory.join("member.jsonl");
        let bulk = format!(
            "{}OVERRIDE{}",
            "x".repeat(512 * 1024),
            "y".repeat(512 * 1024)
        );
        std::fs::write(
            &member,
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                user_text("prompt", "TOKEN-A"),
                tool_use_event("read", "Read", "/tmp/root"),
                tool_result_event("read", &bulk),
                user_text("second", "TOKEN-B"),
                tool_use_event("edit", "Edit", "/tmp/edited")
            ),
        )
        .unwrap();
        let owner = context("a");
        let template = wide_template(&source);
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[member.to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        CompactGraph {
            source,
            member,
            store,
            owner,
            template,
            graph,
        }
    }

    const OVERRIDE_MATRIX: [((&str, &[&str]), bool); 6] = [
        (("OVERRIDE", &["Read"]), true),
        (("OVERRIDE", &["Edit"]), false),
        (("TOKEN-A", &["Read"]), false),
        (("TOKEN-B", &["Read"]), true),
        (("TOKEN-B", &["Edit"]), false),
        (("MISSING", &[]), false),
    ];

    fn override_answers(fixture: &CompactGraph) -> Vec<Option<bool>> {
        OVERRIDE_MATRIX
            .iter()
            .map(|((token, invalidated_by), _)| {
                value_of(&fixture.ask(json!([]), has_override(token, invalidated_by), None))
            })
            .collect()
    }

    #[test]
    fn index_first_build_writes_only_the_index_and_upgrades_on_the_first_full_query() {
        let fixture = bulk_member_graph();
        let (store, member) = (&fixture.store, &fixture.member);
        let disk = || store.prepared_disk.stats();
        let (builds, writes, written) = (
            store.fact_builds.load(Ordering::Relaxed),
            disk().writes,
            disk().write_bytes,
        );
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_tool("Read"), None)),
            Some(true)
        );
        let key = fixture.member_key();
        let stored = store.prepared_disk.entry_file_len(&key);
        let indexed = index_read_bytes(store, &key);
        assert_eq!(
            (store.fact_builds.load(Ordering::Relaxed), disk().writes),
            (builds + 1, writes + 1)
        );
        assert_eq!(stored, indexed);
        assert!(
            stored < 8 * 1024,
            "the index-only entry holds {stored} bytes"
        );
        assert!(disk().write_bytes - written < 8 * 1024);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        assert_eq!(resident_member_is_full(store, member), Some(false));
        evict_memory_facts(store, member);
        let cold = fixture.ask(json!([]), has_tool("Read"), None);
        assert_eq!(value_of(&cold), Some(true));
        assert_eq!(cache_reads(&cold), (Some(1), Some(indexed), Some(0)));
        assert_eq!(cold["usage"]["cold_parses"].as_u64(), Some(0));
        assert_eq!(cold["usage"]["source_opens"].as_u64(), Some(0));
        assert_eq!(resident_member_is_full(store, member), Some(false));
        let root_key = fixture.root_key();
        let root_indexed = index_read_bytes(store, &root_key);
        assert_eq!(store.prepared_disk.entry_file_len(&root_key), root_indexed);
        let text = fixture.ask(json!([]), has_override("OVERRIDE", &["Read"]), None);
        assert_eq!(value_of(&text), Some(true));
        assert_eq!(
            cache_reads(&text),
            (Some(2), Some(root_indexed + indexed), Some(0))
        );
        assert_eq!(text["usage"]["cold_parses"].as_u64(), Some(0));
        assert_eq!(text["usage"]["source_opens"].as_u64(), Some(1));
        assert_eq!(
            (store.fact_builds.load(Ordering::Relaxed), disk().writes),
            (builds + 3, writes + 3)
        );
        let upgraded = store.prepared_disk.entry_file_len(&key);
        assert!(
            upgraded > 1024 * 1024,
            "the upgraded entry holds {upgraded} bytes"
        );
        assert_eq!(
            (
                store.prepared_disk.entry_text_state(&key),
                store.prepared_disk.entry_text_state(&root_key)
            ),
            (Some(TextState::Present), Some(TextState::Present))
        );
        assert_eq!(
            disk().bytes as u64,
            upgraded + store.prepared_disk.entry_file_len(&root_key)
        );
        assert_eq!(resident_member_is_full(store, member), Some(true));
        let again = fixture.ask(json!([]), has_override("OVERRIDE", &["Read"]), None);
        assert_eq!(value_of(&again), Some(true));
        assert_eq!(cache_reads(&again), (Some(0), Some(0), Some(0)));
        let hot = fixture.ask(json!([]), has_tool("Read"), None);
        assert_eq!(value_of(&hot), Some(true));
        assert_eq!(cache_reads(&hot), (Some(0), Some(0), Some(0)));
        let expected: Vec<Option<bool>> = OVERRIDE_MATRIX
            .iter()
            .map(|(_, expected)| Some(*expected))
            .collect();
        assert_eq!(override_answers(&fixture), expected);
        evict_memory_facts(store, member);
        let reread = fixture.ask(json!([]), has_override("OVERRIDE", &["Read"]), None);
        assert_eq!(value_of(&reread), Some(true));
        assert_eq!(cache_reads(&reread), (Some(1), Some(upgraded), Some(0)));
        assert_eq!(override_answers(&fixture), expected);
        assert_eq!(
            (store.fact_builds.load(Ordering::Relaxed), disk().writes),
            (builds + 3, writes + 3)
        );
        store.assert_conserved();
    }

    #[test]
    fn duplicate_full_builds_commit_one_upgrade_and_share_the_answer() {
        let fixture = bulk_member_graph();
        let (store, member) = (&fixture.store, &fixture.member);
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_tool("Read"), None)),
            Some(true)
        );
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_override("root-prompt", &[]), None)),
            Some(true)
        );
        let key = fixture.member_key();
        let indexed = index_read_bytes(store, &key);
        assert_eq!(
            (
                store.prepared_disk.entry_text_state(&key),
                store.prepared_disk.entry_text_state(&fixture.root_key())
            ),
            (Some(TextState::Unbuilt), Some(TextState::Present))
        );
        evict_memory_facts(store, member);
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        *store.built_facts_hook.lock().unwrap() = Some(Arc::new(
            move |_: &Arc<crate::snapshot_prepared::PreparedFacts>| {
                paused_tx.send(()).unwrap();
                resume_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the duplicate build barrier was never released");
            },
        ));
        let (builds, writes, lookups) = (
            store.fact_builds.load(Ordering::Relaxed),
            store.prepared_disk.stats().writes,
            store.fact_lookups.load(Ordering::Relaxed),
        );
        let request = prepared_graph_request(
            &fixture.graph,
            &fixture.template,
            json!([]),
            has_override("OVERRIDE", &["Read"]),
            None,
        );
        let (first, index, second) = std::thread::scope(|scope| {
            let first =
                scope.spawn(|| store.request(&request, &fixture.owner, &Cancellation::default()));
            paused_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the first builder never projected");
            let index = fixture.ask(json!([]), has_tool("Edit"), None);
            let second = store.request(&request, &fixture.owner, &Cancellation::default());
            resume_tx.send(()).unwrap();
            (
                single_step(first.join().unwrap()),
                index,
                single_step(second),
            )
        });
        assert_eq!(
            (value_of(&first), value_of(&index), value_of(&second)),
            (Some(true), Some(true), Some(true))
        );
        for reply in [&first, &index, &second] {
            assert_eq!(
                cache_reads(reply),
                (Some(1), Some(indexed), Some(0)),
                "{reply:?}"
            );
        }
        assert_eq!(
            (
                store.fact_builds.load(Ordering::Relaxed),
                store.prepared_disk.stats().writes,
                store.fact_lookups.load(Ordering::Relaxed)
            ),
            (builds + 2, writes + 1, lookups + 3)
        );
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Present)
        );
        assert_eq!(resident_member_is_full(store, member), Some(true));
        assert_eq!(store.lock_state().transient_bytes, 0);
        let expected: Vec<Option<bool>> = OVERRIDE_MATRIX
            .iter()
            .map(|(_, expected)| Some(*expected))
            .collect();
        assert_eq!(override_answers(&fixture), expected);
        store.assert_conserved();
    }

    #[test]
    fn oversized_duplicate_full_builds_commit_one_upgrade_and_keep_the_compact_resident() {
        let fixture = bulk_member_graph_with(NativeStore::new(&json!({"max_read_bytes_per_step":2*1024*1024,"max_entry_bytes":4*1024*1024,"max_retained_bytes":64*1024*1024,"max_prepared_fact_memory_bytes":256*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap());
        let (store, member) = (&fixture.store, &fixture.member);
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_tool("Read"), None)),
            Some(true)
        );
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_override("root-prompt", &[]), None)),
            Some(true)
        );
        let key = fixture.member_key();
        let indexed = index_read_bytes(store, &key);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        assert_eq!(resident_member_is_full(store, member), Some(false));
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        *store.built_facts_hook.lock().unwrap() = Some(Arc::new(
            move |_: &Arc<crate::snapshot_prepared::PreparedFacts>| {
                paused_tx.send(()).unwrap();
                resume_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the duplicate build barrier was never released");
            },
        ));
        let (builds, writes, lookups) = (
            store.fact_builds.load(Ordering::Relaxed),
            store.prepared_disk.stats().writes,
            store.fact_lookups.load(Ordering::Relaxed),
        );
        let request = prepared_graph_request(
            &fixture.graph,
            &fixture.template,
            json!([]),
            has_override("OVERRIDE", &["Read"]),
            None,
        );
        let (first, second) = std::thread::scope(|scope| {
            let first =
                scope.spawn(|| store.request(&request, &fixture.owner, &Cancellation::default()));
            paused_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the first builder never projected");
            let second = store.request(&request, &fixture.owner, &Cancellation::default());
            resume_tx.send(()).unwrap();
            (single_step(first.join().unwrap()), single_step(second))
        });
        assert_eq!(
            (value_of(&first), value_of(&second)),
            (Some(true), Some(true))
        );
        let upgraded = store.prepared_disk.entry_file_len(&key);
        let mut reads = [cache_reads(&first), cache_reads(&second)];
        reads.sort();
        assert_eq!(
            reads,
            [
                (Some(1), Some(indexed), Some(0)),
                (Some(2), Some(indexed + upgraded), Some(0))
            ]
        );
        assert_eq!(
            (
                store.fact_builds.load(Ordering::Relaxed),
                store.prepared_disk.stats().writes,
                store.fact_lookups.load(Ordering::Relaxed)
            ),
            (builds + 2, writes + 1, lookups + 3)
        );
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Present)
        );
        assert_eq!(resident_member_is_full(store, member), Some(false));
        assert_eq!(store.lock_state().transient_bytes, 0);
        let expected: Vec<Option<bool>> = OVERRIDE_MATRIX
            .iter()
            .map(|(_, expected)| Some(*expected))
            .collect();
        assert_eq!(override_answers(&fixture), expected);
        assert_eq!(resident_member_is_full(store, member), Some(false));
        store.assert_conserved();
    }

    #[test]
    fn override_answers_are_identical_across_index_and_full_loads() {
        let fixture = bulk_member_graph();
        let expected: Vec<Option<bool>> = OVERRIDE_MATRIX
            .iter()
            .map(|(_, expected)| Some(*expected))
            .collect();
        assert_eq!(override_answers(&fixture), expected);
        assert_eq!(
            resident_member_is_full(&fixture.store, &fixture.member),
            Some(true)
        );
        evict_memory_facts(&fixture.store, &fixture.member);
        let lookups = fixture.store.fact_lookups.load(Ordering::Relaxed);
        assert_eq!(override_answers(&fixture), expected);
        assert_eq!(
            fixture.store.fact_lookups.load(Ordering::Relaxed),
            lookups + 1
        );
        evict_memory_facts(&fixture.store, &fixture.member);
        assert_eq!(
            value_of(&fixture.ask(json!([]), has_tool("Read"), None)),
            Some(true)
        );
        assert_eq!(
            resident_member_is_full(&fixture.store, &fixture.member),
            Some(false)
        );
        assert_eq!(override_answers(&fixture), expected);
        assert_eq!(
            resident_member_is_full(&fixture.store, &fixture.member),
            Some(true)
        );
        assert_eq!(
            fixture.store.fact_lookups.load(Ordering::Relaxed),
            lookups + 3
        );
        fixture.store.assert_conserved();
    }

    #[test]
    fn full_waiter_joining_an_index_read_upgrades_to_the_text_section() {
        let source = Source::new(&format!("{}\n", user_text("root", "root-prompt")));
        let members = registered_members(&source, &["thread-a"]);
        let store = Arc::new(fact_budget_store(16 * 1024 * 1024));
        let owner = background_owner();
        warm_registered_pass(&store, &source, &["thread-a"], &owner);
        let key = fact_entry_key(&members[0], &owner);
        let indexed = index_read_bytes(&store, &key);
        assert_eq!(store.prepared_disk.entry_file_len(&key), indexed);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        let template = acquire(&source.path);
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[members[0].to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let answer = |query: Value, observers: Option<Value>| {
            single_step(store.request(
                &prepared_graph_request(&graph, &template, json!([]), query, observers),
                &owner,
                &Cancellation::default(),
            ))
        };
        let root_indexed = index_read_bytes(&store, &fact_entry_key(&source.path, &owner));
        let (builds, writes) = (
            store.fact_builds.load(Ordering::Relaxed),
            store.prepared_disk.stats().writes,
        );
        let primed = answer(has_override("hello", &["Read"]), None);
        assert_eq!(value_of(&primed), Some(true));
        assert_eq!(
            cache_reads(&primed),
            (Some(2), Some(root_indexed + indexed), Some(0))
        );
        assert_eq!(
            (
                store.fact_builds.load(Ordering::Relaxed),
                store.prepared_disk.stats().writes
            ),
            (builds + 2, writes + 2)
        );
        assert_eq!(resident_member_is_full(&store, &members[0]), Some(true));
        let stored = store.prepared_disk.entry_file_len(&key);
        assert!(indexed < stored);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Present)
        );
        forget_member_facts(&store, &members);
        let lookups = store.fact_lookups.load(Ordering::Relaxed);
        let gate = install_fact_read_gate(&store, || {});
        let deadline = now_ms() + 30_000;
        let index_reader = spawn_warm(
            &store,
            warm_request(&source, &["thread-a"], deadline),
            &owner,
            Cancellation::default(),
        );
        gate.wait_until_read();
        let full_reader = spawn_warm(
            &store,
            prepared_graph_request(
                &graph,
                &template,
                json!([]),
                has_override("hello", &["Read"]),
                None,
            ),
            &owner,
            Cancellation::default(),
        );
        wait_for_fact_read_waiter(&store);
        gate.release();
        let (index_reply, full_reply) = (
            warm_reply(&index_reader),
            single_step(warm_reply(&full_reader)),
        );
        store.prepared_disk.set_read_hook(None);
        assert_eq!(
            index_reply["status"].as_str(),
            Some("ok"),
            "{index_reply:?}"
        );
        assert_eq!(index_reply["data"]["complete"].as_bool(), Some(true));
        assert_eq!(cache_reads(&index_reply), (Some(1), Some(indexed), Some(0)));
        assert_eq!(value_of(&full_reply), Some(true));
        assert_eq!(cache_reads(&full_reply), (Some(1), Some(stored), Some(0)));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), lookups + 2);
        assert_eq!(store.fact_read_waits.load(Ordering::Relaxed), 1);
        assert_eq!(resident_member_is_full(&store, &members[0]), Some(true));
        assert!(!store.lock_state().fact_read_in_flight);
        let repeat = answer(has_override("hello", &["Read"]), None);
        assert_eq!(value_of(&repeat), Some(true));
        assert_eq!(cache_reads(&repeat), (Some(0), Some(0), Some(0)));
        forget_member_facts(&store, &members);
        let observed = answer(
            has_tool("Read"),
            Some(json!([has_override("hello", &["Read"])])),
        );
        assert_eq!(observed["status"].as_str(), Some("ok"), "{observed:?}");
        assert_eq!(
            observed["data"],
            json!({"kind":"observed_scalar","value":false,"observers":[true]})
        );
        assert_eq!(cache_reads(&observed), (Some(1), Some(stored), Some(0)));
        assert_eq!(resident_member_is_full(&store, &members[0]), Some(true));
        forget_member_facts(&store, &members);
        let plain = answer(has_tool("Read"), Some(json!([has_tool("Edit")])));
        assert_eq!(plain["status"].as_str(), Some("ok"), "{plain:?}");
        assert_eq!(
            plain["data"],
            json!({"kind":"observed_scalar","value":false,"observers":[false]})
        );
        assert_eq!(cache_reads(&plain), (Some(1), Some(indexed), Some(0)));
        assert_eq!(resident_member_is_full(&store, &members[0]), Some(false));
        store.assert_conserved();
    }

    #[test]
    fn repeated_root_predicates_keep_scope_metadata_typed() {
        let skill = json!({"type":"assistant","uuid":"skill","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{"model":"m","content":[{"type":"tool_use","id":"skill","name":"Skill","input":{"skill":"skill-4"}}]}});
        let source = Source::new(&format!("{}\n{skill}\n", user_text("prompt", "ready")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let template = acquire(&source.path);
        let root = finish(
            &store,
            store.request(&template, &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let before = store.description_builds.load(Ordering::Relaxed);
        for index in 0..24 {
            let request = prepared_graph_request(
                &graph,
                &template,
                json!([]),
                json!({"kind":"has_skill","values":[format!("skill-{index}")],"subagents":true}),
                None,
            );
            let answer = single_step(store.request(&request, &owner, &Cancellation::default()));
            assert_eq!(value_of(&answer), Some(index == 4), "{answer:?}");
        }
        assert_eq!(store.description_builds.load(Ordering::Relaxed), before);
        let (_, description) = store
            .pin_scope_for_work(
                &handle(&root),
                &owner,
                template["deadline_unix_ms"].as_u64().unwrap(),
            )
            .unwrap();
        assert_eq!(store.description_builds.load(Ordering::Relaxed), before + 1);
        assert_eq!(
            description["classifier"],
            json!({"id":"native","version":"1"})
        );
        assert_eq!(description["model"].as_str(), Some("m"));
        assert_eq!(
            description["canonical_path"].as_str(),
            std::fs::canonicalize(&source.path).unwrap().to_str()
        );
        let later = json!({"type":"assistant","uuid":"later","sessionId":"s","timestamp":"2026-01-02T03:04:07Z","message":{"model":"later","content":[]}});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source.path)
            .unwrap()
            .write_all(format!("{later}\n").as_bytes())
            .unwrap();
        let newer = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            store.pin(&handle(&root), &owner).unwrap().model(),
            Some("m")
        );
        assert_eq!(
            store.pin(&handle(&newer), &owner).unwrap().model(),
            Some("later")
        );
        store.assert_conserved();
    }

    #[test]
    fn selected_root_slices_answer_has_override_within_their_view() {
        for observer_first in [false, true] {
            selected_root_slice_upgrade(observer_first);
        }
    }

    fn selected_root_slice_upgrade(observer_first: bool) {
        let source = Source::new(&format!(
            "{}\n{}\n",
            user_text("first", "TOKEN-ROOT"),
            user_text("second", "SECOND")
        ));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let template = acquire(&source.path);
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let ask = |selectors: Value, query: Value, observers: Option<Value>| {
            single_step(store.request(
                &prepared_graph_request(&graph, &template, selectors, query, observers),
                &owner,
                &Cancellation::default(),
            ))
        };
        let current_turn = || json!([{"kind":"current_turn"}]);
        let root_key = fact_entry_key(&source.path, &owner);
        let lookups = store.fact_lookups.load(Ordering::Relaxed);
        let builds = store.fact_builds.load(Ordering::Relaxed);
        let indexed = ask(current_turn(), has_tool("Missing"), None);
        assert_eq!(value_of(&indexed), Some(false));
        {
            let state = store.lock_state();
            let graph = state.prepared_graphs
                [graph["data"]["handle"]["graph_id"].as_str().unwrap()]
            .lock()
            .unwrap();
            let facts = graph.root_slices.values().next().unwrap();
            assert!(facts.serves(FactsNeed::Index));
            assert!(!facts.serves(FactsNeed::Full));
        }
        let scoped = if observer_first {
            ask(
                current_turn(),
                has_tool("Missing"),
                Some(json!([has_override("TOKEN-ROOT", &[])])),
            )
        } else {
            ask(current_turn(), has_override("TOKEN-ROOT", &[]), None)
        };
        assert_eq!(value_of(&scoped), Some(false));
        if observer_first {
            assert_eq!(scoped["data"]["observers"], json!([false]));
        }
        assert_eq!(
            (
                store.fact_lookups.load(Ordering::Relaxed),
                store.fact_builds.load(Ordering::Relaxed)
            ),
            (lookups, builds + 2)
        );
        assert_eq!(
            store.prepared_disk.entry_text_state(&root_key),
            Some(TextState::Unbuilt)
        );
        let whole = ask(json!([]), has_override("TOKEN-ROOT", &[]), None);
        assert_eq!(value_of(&whole), Some(true));
        assert_eq!(
            (
                store.fact_lookups.load(Ordering::Relaxed),
                store.fact_builds.load(Ordering::Relaxed)
            ),
            (lookups + 1, builds + 3)
        );
        assert_eq!(
            store.prepared_disk.entry_text_state(&root_key),
            Some(TextState::Present)
        );
        let again = ask(json!([]), has_override("TOKEN-ROOT", &[]), None);
        assert_eq!(value_of(&again), Some(true));
        assert_eq!(cache_reads(&again), (Some(0), Some(0), Some(0)));
        let observer = || Some(json!([has_override("TOKEN-ROOT", &[])]));
        let scoped = ask(current_turn(), has_tool("Missing"), observer());
        assert_eq!(
            scoped["data"],
            json!({"kind":"observed_scalar","value":false,"observers":[false]}),
            "{scoped:?}"
        );
        let whole = ask(json!([]), has_tool("Missing"), observer());
        assert_eq!(
            whole["data"],
            json!({"kind":"observed_scalar","value":false,"observers":[true]}),
            "{whole:?}"
        );
        assert_eq!(
            store.fact_builds.load(Ordering::Relaxed),
            builds + 3,
            "a selected view rebuilt its facts"
        );
        evict_memory_facts(&store, &source.path);
        let stored = store.prepared_disk.entry_file_len(&root_key);
        let scoped = ask(current_turn(), has_override("TOKEN-ROOT", &[]), None);
        assert_eq!(value_of(&scoped), Some(false));
        assert_eq!(cache_reads(&scoped), (Some(0), Some(0), Some(0)));
        let whole = ask(json!([]), has_override("TOKEN-ROOT", &[]), None);
        assert_eq!(value_of(&whole), Some(true));
        assert_eq!(cache_reads(&whole), (Some(1), Some(stored), Some(0)));
        let observed = ask(current_turn(), has_tool("Missing"), observer());
        assert_eq!(
            observed["data"],
            json!({"kind":"observed_scalar","value":false,"observers":[false]}),
            "{observed:?}"
        );
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds + 3);
        store.assert_conserved();
    }

    #[test]
    fn restarted_owner_serves_index_reads_from_inherited_entries() {
        let namespace = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "cc-transcript-compact-inherited-{}-{}",
                std::process::id(),
                now_ms()
            ));
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = Some(namespace.clone()));
        let source = Source::new(&format!("{}\n", user("root")));
        let ids = ["thread-a", "thread-b"];
        let members = registered_members(&source, &ids);
        let owner = background_owner();
        let producing = || {
            NativeStore::new(&json!({"prepared_fact_producer":"compact-inherited-test","max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":16*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap()
        };
        let first = producing();
        assert!(warm_registered_pass(&first, &source, &ids, &owner).2 > 0);
        let key = fact_entry_key(&members[0], &owner);
        let (stored, indexed) = (
            first.prepared_disk.entry_file_len(&key),
            index_read_bytes(&first, &key),
        );
        assert_eq!(indexed, stored);
        drop(first);
        let restarted = producing();
        assert_eq!(
            warm_registered_pass(&restarted, &source, &ids, &owner),
            (2, 2 * indexed, 0)
        );
        assert_eq!(
            resident_member_is_full(&restarted, &members[0]),
            Some(false)
        );
        assert_eq!(restarted.prepared_disk.entry_file_len(&key), stored);
        restarted.assert_conserved();
        drop(restarted);
        crate::snapshot_prepared_disk::REUSE_NAMESPACE_OVERRIDE
            .with(|path| *path.borrow_mut() = None);
        std::fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn a_full_entry_under_another_binding_never_serves() {
        let source = Source::new(&format!("{}\n", user("root")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let bounds = limits(&acquire(&source.path)).unwrap();
        let (a, b) = (binding(&store, "fetch_a"), binding(&store, "fetch_b"));
        let root = pinned_root(&store, &source, &a);
        let full = root_facts_read_with(&store, &root, &a, &bounds, FactsNeed::Full).0;
        assert!(full.is_full());
        let classifier = json!({"id":"native","version":"1"});
        let touch = |owner: &Value, need: FactsNeed| {
            store
                .lock_state()
                .touch_prepared_facts(
                    &fact_entry_key(&source.path, owner),
                    root.stamp,
                    owner["registry_generation"].as_str().unwrap(),
                    "hook",
                    &owner["authority"],
                    &classifier,
                    need,
                )
                .is_some()
        };
        assert!(touch(&a, FactsNeed::Full) && touch(&a, FactsNeed::Index));
        assert!(!touch(&b, FactsNeed::Full) && !touch(&b, FactsNeed::Index));
        let builds = store.fact_builds.load(Ordering::Relaxed);
        let other = root_facts_read_with(&store, &root, &b, &bounds, FactsNeed::Full).0;
        assert!(other.is_full() && !Arc::ptr_eq(&full, &other));
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds + 1);
        assert_eq!(resident_bindings(&store, &root), 2);
        let compact = root_facts_read_with(&store, &root, &a, &bounds, FactsNeed::Index).0;
        assert!(Arc::ptr_eq(&compact, &full));
        drop((full, other, compact));
        store.assert_conserved();
    }

    #[test]
    fn unbuilt_root_upgrades_from_the_pinned_snapshot_without_reparsing() {
        let source = Source::new(&format!("{}\n", user_text("root", "TOKEN-ROOT")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let template = acquire(&source.path);
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let ask = |query: Value| {
            single_step(store.request(
                &prepared_graph_request(&graph, &template, json!([]), query, None),
                &owner,
                &Cancellation::default(),
            ))
        };
        let key = fact_entry_key(&source.path, &owner);
        let indexed = index_read_bytes(&store, &key);
        assert_eq!(store.prepared_disk.entry_file_len(&key), indexed);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        evict_memory_facts(&store, &source.path);
        source.append(&format!("{}\n", user_text("later", "TOKEN-LATE")));
        let appended = ask(has_tool("Missing"));
        assert_eq!(value_of(&appended), Some(false));
        assert_eq!(cache_reads(&appended).0, Some(0));
        assert_eq!(appended["usage"]["cold_parses"].as_u64(), Some(0));
        assert_eq!(
            (
                appended["usage"]["source_opens"].as_u64(),
                appended["usage"]["source_bytes_read"].as_u64()
            ),
            (Some(2), Some(128))
        );
        let (builds, writes) = (
            store.fact_builds.load(Ordering::Relaxed),
            store.prepared_disk.stats().writes,
        );
        let full = ask(has_override("TOKEN-ROOT", &[]));
        assert_eq!(value_of(&full), Some(true));
        assert_eq!(cache_reads(&full), (Some(1), Some(indexed), Some(64)));
        assert_eq!(
            (
                full["usage"]["cold_parses"].as_u64(),
                full["usage"]["source_opens"].as_u64()
            ),
            (Some(0), Some(1))
        );
        assert_eq!(
            (
                store.fact_builds.load(Ordering::Relaxed),
                store.prepared_disk.stats().writes
            ),
            (builds + 1, writes + 1)
        );
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Present)
        );
        assert_eq!(
            store.prepared_disk.stats().bytes as u64,
            store.prepared_disk.entry_file_len(&key)
        );
        let late = ask(has_override("TOKEN-LATE", &[]));
        assert_eq!(value_of(&late), Some(false));
        assert_eq!(value_of(&ask(has_override("TOKEN-ROOT", &[]))), Some(true));
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds + 1);
        store.assert_conserved();
    }

    #[test]
    fn a_cancelled_full_upgrader_publishes_nothing_and_the_next_caller_upgrades() {
        let source = Source::new(&format!("{}\n", user_text("root", "root-prompt")));
        let members = registered_members(&source, &["thread-a"]);
        let store = Arc::new(fact_budget_store(16 * 1024 * 1024));
        let owner = background_owner();
        warm_registered_pass(&store, &source, &["thread-a"], &owner);
        let key = fact_entry_key(&members[0], &owner);
        let indexed = index_read_bytes(&store, &key);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        let template = acquire(&source.path);
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let graph = finish(
            &store,
            store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":[members[0].to_string_lossy().as_ref()],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        let request =
            |query: Value| prepared_graph_request(&graph, &template, json!([]), query, None);
        let primed = single_step(store.request(
            &request(has_override("root-prompt", &[])),
            &owner,
            &Cancellation::default(),
        ));
        assert_eq!(value_of(&primed), Some(true));
        forget_member_facts(&store, &members);
        let writes = store.prepared_disk.stats().writes;
        let gate = install_fact_read_gate(&store, || {});
        let cancellation = Cancellation::default();
        let winner = spawn_warm(
            &store,
            request(has_override("hello", &["Read"])),
            &owner,
            cancellation.clone(),
        );
        gate.wait_until_read();
        cancellation.cancel();
        gate.release();
        let cancelled = warm_reply(&winner);
        store.prepared_disk.set_read_hook(None);
        assert_eq!(
            cancelled["status"].as_str(),
            Some("cancelled"),
            "{cancelled:?}"
        );
        assert_eq!(store.prepared_disk.stats().writes, writes);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Unbuilt)
        );
        assert_eq!(store.prepared_disk.entry_file_len(&key), indexed);
        assert!(!store.lock_state().fact_read_in_flight);
        assert_eq!(store.lock_state().transient_bytes, 0);
        let upgraded = single_step(store.request(
            &request(has_override("hello", &["Read"])),
            &owner,
            &Cancellation::default(),
        ));
        assert_eq!(value_of(&upgraded), Some(true));
        assert_eq!(cache_reads(&upgraded), (Some(1), Some(indexed), Some(0)));
        assert_eq!(store.prepared_disk.stats().writes, writes + 1);
        assert_eq!(
            store.prepared_disk.entry_text_state(&key),
            Some(TextState::Present)
        );
        assert_eq!(resident_member_is_full(&store, &members[0]), Some(true));
        store.assert_conserved();
    }

    fn pinned_root(store: &NativeStore, source: &Source, owner: &Value) -> Arc<TranscriptSnapshot> {
        let completed = finish(
            store,
            store.request(&acquire(&source.path), owner, &Cancellation::default()),
            owner,
        );
        store.pin(handle(&completed), owner).unwrap()
    }

    fn root_facts_read(
        store: &NativeStore,
        root: &Arc<TranscriptSnapshot>,
        owner: &Value,
        bounds: &WorkLimits,
    ) -> (Arc<crate::snapshot_prepared::PreparedFacts>, u64) {
        root_facts_read_with(store, root, owner, bounds, FactsNeed::Index)
    }

    fn root_facts_read_with(
        store: &NativeStore,
        root: &Arc<TranscriptSnapshot>,
        owner: &Value,
        bounds: &WorkLimits,
        need: FactsNeed,
    ) -> (Arc<crate::snapshot_prepared::PreparedFacts>, u64) {
        let mut usage = [0u64; 20];
        let (retained, reservation) = store
            .prepared_root_facts(
                root,
                &json!({"id":"native","version":"1"}),
                owner,
                bounds,
                PreparedCacheReads::Unmetered,
                &Cancellation::default(),
                &mut usage,
                need,
            )
            .unwrap();
        let facts = Arc::clone(retained.facts());
        drop(retained);
        drop(reservation);
        (facts, usage[18])
    }

    #[test]
    fn concurrent_identical_root_fact_cold_reads_share_one_disk_read() {
        let source = Source::new(&format!("{}\n{}\n", user("root"), user("child")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let bounds = limits(&acquire(&source.path)).unwrap();
        let root = pinned_root(&store, &source, &owner);
        assert_eq!(root_facts_read(&store, &root, &owner, &bounds).1, 0);
        assert_eq!(
            store
                .lock_state()
                .remove_identity_facts(root.stamp.identity),
            1
        );
        assert!(store
            .prepared_disk
            .has_entry(&fact_entry_key(&source.path, &owner))
            .unwrap());
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        let gate = install_fact_read_gate(&store, || {});
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| root_facts_read(&store, &root, &owner, &bounds));
            gate.wait_until_read();
            let second = scope.spawn(|| root_facts_read(&store, &root, &owner, &bounds));
            wait_for_fact_read_waiter(&store);
            gate.release();
            (first.join().unwrap(), second.join().unwrap())
        });
        store.prepared_disk.set_read_hook(None);
        assert_eq!((first.1, second.1), (1, 0));
        assert!(Arc::ptr_eq(&first.0, &second.0));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert_eq!(store.fact_read_waits.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(
            &store
                .lock_state()
                .cached_prepared_facts(&root.stamp.identity)
                .facts,
            &first.0
        ));
        drop((first, second));
        store.assert_conserved();
    }

    #[test]
    fn a_cancelled_root_fact_read_keeps_the_entry_and_builds_nothing() {
        let source = Source::new(&format!("{}\n{}\n", user("root"), user("child")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let bounds = limits(&acquire(&source.path)).unwrap();
        let root = pinned_root(&store, &source, &owner);
        assert_eq!(root_facts_read(&store, &root, &owner, &bounds).1, 0);
        assert_eq!(
            store
                .lock_state()
                .remove_identity_facts(root.stamp.identity),
            1
        );
        let key = fact_entry_key(&source.path, &owner);
        let builds = store.fact_builds.load(Ordering::Relaxed);
        let cancellation = Cancellation::default();
        let stop = cancellation.clone();
        store
            .prepared_disk
            .set_read_hook(Some(Box::new(move || stop.cancel())));
        let mut usage = [0u64; 20];
        let stopped = store
            .prepared_root_facts(
                &root,
                &json!({"id":"native","version":"1"}),
                &owner,
                &bounds,
                PreparedCacheReads::Unmetered,
                &cancellation,
                &mut usage,
                FactsNeed::Index,
            )
            .err()
            .map(|error| error.status);
        store.prepared_disk.set_read_hook(None);
        assert_eq!(stopped, Some(Status::Cancelled));
        assert_eq!((usage[18], usage[19]), (0, 0));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 1);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds);
        assert!(store.prepared_disk.has_entry(&key).unwrap());
        assert!(!store.lock_state().fact_read_in_flight);
        store.assert_conserved();
        assert_eq!(root_facts_read(&store, &root, &owner, &bounds).1, 1);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds);
        store.assert_conserved();
    }

    #[test]
    fn waiting_reader_with_a_different_binding_never_reuses_the_published_facts() {
        let source = Source::new(&format!("{}\n{}\n", user("root"), user("child")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let owner = context("a");
        let other = json!({"claimant":"a","admission":"hook","authority":owner["authority"],"registry_generation":"other-registry"});
        let bounds = limits(&acquire(&source.path)).unwrap();
        let root = pinned_root(&store, &source, &owner);
        for binding in [&owner, &other] {
            assert_eq!(root_facts_read(&store, &root, binding, &bounds).1, 0);
            assert_eq!(
                store
                    .lock_state()
                    .remove_identity_facts(root.stamp.identity),
                1
            );
            assert!(store
                .prepared_disk
                .has_entry(&fact_entry_key(&source.path, binding))
                .unwrap());
        }
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        let gate = install_fact_read_gate(&store, || {});
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| root_facts_read(&store, &root, &owner, &bounds));
            gate.wait_until_read();
            let second = scope.spawn(|| root_facts_read(&store, &root, &other, &bounds));
            wait_for_fact_read_waiter(&store);
            gate.release();
            (first.join().unwrap(), second.join().unwrap())
        });
        store.prepared_disk.set_read_hook(None);
        assert_eq!((first.1, second.1), (1, 1));
        assert!(!Arc::ptr_eq(&first.0, &second.0));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 2);
        assert_eq!(store.fact_read_waits.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(
            &resident_binding(&store, &root, &owner),
            &first.0
        ));
        assert!(Arc::ptr_eq(
            &resident_binding(&store, &root, &other),
            &second.0
        ));
        assert_eq!(resident_bindings(&store, &root), 2);
        drop((first, second));
        store.assert_conserved();
    }

    fn binding(store: &NativeStore, tool: &str) -> Value {
        let owner = context("a");
        let registry_generation = store
            .register_tool_registry(
                &json!([{"name":tool,"behaves_like":"Read","span_edit":null}]),
                &owner,
            )
            .unwrap();
        json!({"claimant":"a","admission":"hook","authority":owner["authority"],"registry_generation":registry_generation})
    }

    fn resident_bindings(store: &NativeStore, root: &Arc<TranscriptSnapshot>) -> usize {
        store
            .lock_state()
            .identity_facts(root.stamp.identity)
            .count()
    }

    fn resident_binding(
        store: &NativeStore,
        root: &Arc<TranscriptSnapshot>,
        owner: &Value,
    ) -> Arc<crate::snapshot_prepared::PreparedFacts> {
        let key = fact_entry_key(&root.canonical_path, owner);
        let state = store.lock_state();
        let cached = state.prepared_facts.get(&key).expect("resident binding");
        assert_eq!(cached.stamp, root.stamp);
        Arc::clone(&cached.facts)
    }

    #[test]
    fn alternating_bindings_on_one_source_read_disk_once_each() {
        let source = Source::new(&format!("{}\n{}\n", user("root"), user("child")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let bounds = limits(&acquire(&source.path)).unwrap();
        let (a, b) = (binding(&store, "fetch_a"), binding(&store, "fetch_b"));
        let root = pinned_root(&store, &source, &a);
        for owner in [&a, &b] {
            assert_eq!(root_facts_read(&store, &root, owner, &bounds).1, 0);
        }
        assert_eq!(
            store
                .lock_state()
                .remove_identity_facts(root.stamp.identity),
            2
        );
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        let reads: Vec<u64> = [&a, &b, &a, &b, &a, &b]
            .into_iter()
            .map(|owner| root_facts_read(&store, &root, owner, &bounds).1)
            .collect();
        assert_eq!(reads, [1, 1, 0, 0, 0, 0]);
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 2);
        assert_eq!(resident_bindings(&store, &root), 2);
        let (first_a, first_b) = (
            resident_binding(&store, &root, &a),
            resident_binding(&store, &root, &b),
        );
        assert!(!Arc::ptr_eq(&first_a, &first_b));
        assert!(Arc::ptr_eq(
            &root_facts_read(&store, &root, &a, &bounds).0,
            &first_a
        ));
        assert!(Arc::ptr_eq(
            &root_facts_read(&store, &root, &b, &bounds).0,
            &first_b
        ));
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 2);
        drop((first_a, first_b));
        store.assert_conserved();
    }

    #[test]
    fn every_binding_of_one_source_stays_resident_while_its_bytes_fit() {
        let source = Source::new(&format!("{}\n", user("root")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let bounds = limits(&acquire(&source.path)).unwrap();
        let bindings: Vec<Value> = (0..17)
            .map(|index| binding(&store, &format!("fetch_{index}")))
            .collect();
        let root = pinned_root(&store, &source, &bindings[0]);
        let published: Vec<_> = bindings
            .iter()
            .map(|owner| root_facts_read(&store, &root, owner, &bounds).0)
            .collect();
        assert_eq!(resident_bindings(&store, &root), bindings.len());
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), bindings.len());
        for (owner, facts) in bindings.iter().zip(&published) {
            let again = root_facts_read(&store, &root, owner, &bounds);
            assert_eq!(again.1, 0);
            assert!(Arc::ptr_eq(&again.0, facts));
            assert!(Arc::ptr_eq(&resident_binding(&store, &root, owner), facts));
        }
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        assert!(published
            .iter()
            .enumerate()
            .all(|(index, facts)| published[..index]
                .iter()
                .all(|earlier| !Arc::ptr_eq(earlier, facts))));
        drop(published);
        store.assert_conserved();
    }

    #[test]
    fn stale_source_revision_rejects_every_cached_binding() {
        let source = Source::new(&format!("{}\n", user("root")));
        let store = fact_budget_store(16 * 1024 * 1024);
        let bounds = limits(&acquire(&source.path)).unwrap();
        let (a, b) = (binding(&store, "fetch_a"), binding(&store, "fetch_b"));
        let root = pinned_root(&store, &source, &a);
        let stale: Vec<_> = [&a, &b]
            .into_iter()
            .map(|owner| root_facts_read(&store, &root, owner, &bounds).0)
            .collect();
        assert_eq!(resident_bindings(&store, &root), 2);
        let stale_key = fact_entry_key(&source.path, &a);
        source.append(&format!("{}\n", user("child")));
        let revised = pinned_root(&store, &source, &a);
        assert_eq!(revised.stamp.identity, root.stamp.identity);
        assert_ne!(revised.stamp, root.stamp);
        let builds = store.fact_builds.load(Ordering::Relaxed);
        let fresh_a = root_facts_read(&store, &revised, &a, &bounds);
        assert_eq!(fresh_a.1, 0);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds + 1);
        assert_eq!(resident_bindings(&store, &revised), 1);
        assert!(Arc::ptr_eq(
            &resident_binding(&store, &revised, &a),
            &fresh_a.0
        ));
        assert!(stale.iter().all(|facts| !Arc::ptr_eq(facts, &fresh_a.0)));
        let fresh_b = root_facts_read(&store, &revised, &b, &bounds);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), builds + 2);
        assert_eq!(resident_bindings(&store, &revised), 2);
        assert!(stale.iter().all(|facts| !Arc::ptr_eq(facts, &fresh_b.0)));
        assert!(store
            .lock_state()
            .touch_prepared_facts(
                &stale_key,
                root.stamp,
                a["registry_generation"].as_str().unwrap(),
                "hook",
                &a["authority"],
                &json!({"id":"native","version":"1"}),
                FactsNeed::Index,
            )
            .is_none());
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), 0);
        drop((stale, fresh_a, fresh_b));
        store.assert_conserved();
    }

    #[test]
    fn metered_warming_never_completes_over_a_missing_or_replaced_entry() {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let source = Source::new(&format!("{}\n", user("root")));
        let member = source.directory.join("thread-a.jsonl");
        std::fs::write(&member, format!("{}\n", user("a"))).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let template = acquire(&source.path);
        let warm = |budget: u64| {
            let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":["thread-a"],"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            request["limits"].insert("max_source_read_bytes", json!(budget));
            store.request(&request, &owner, &Cancellation::default())
        };
        let mut built = warm(1024 * 1024);
        for _ in 0..16 {
            if built["data"]["complete"].as_bool() == Some(true) {
                break;
            }
            built = warm(1024 * 1024);
        }
        assert_eq!(built["data"]["complete"].as_bool(), Some(true), "{built:?}");
        let key = fact_entry_key(&member, &owner);
        let entry = store.prepared_disk.entry_file(&key);
        let stored = store.prepared_disk.entry_file_len(&key);
        let indexed = index_read_bytes(&store, &key);
        let identity = SourceStamp::of(&std::fs::metadata(&member).unwrap()).identity;
        let unlink = |entry: &Path| std::fs::remove_file(entry).unwrap();
        let corrupt = |entry: &Path| {
            let mut bytes = std::fs::read(entry).unwrap();
            bytes[crate::snapshot_prepared_disk::HEADER_BYTES + 8] ^= 1;
            std::fs::OpenOptions::new()
                .write(true)
                .open(entry)
                .unwrap()
                .write_all(&bytes)
                .unwrap();
        };
        let replace = |entry: &Path| {
            let bytes = vec![0u8; std::fs::metadata(entry).unwrap().len() as usize];
            std::fs::remove_file(entry).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(entry)
                .unwrap()
                .write_all(&bytes)
                .unwrap();
        };
        let damages: [(&dyn Fn(&Path), u64); 3] = [(&unlink, 0), (&corrupt, 1), (&replace, 1)];
        for (round, (damage, reads)) in damages.into_iter().enumerate() {
            evict_memory_facts(&store, &member);
            damage(&entry);
            for _ in 0..2 {
                let oversize = warm(indexed - 1);
                assert_eq!(refusal(&oversize), ENTRY_LIMIT, "{oversize:?}");
                assert_eq!(cache_reads(&oversize), (Some(0), Some(0), Some(0)));
            }
            let fitting = warm(indexed);
            assert_eq!(refusal(&fitting), ENTRY_EVICTED, "{fitting:?}");
            assert_eq!(
                cache_reads(&fitting),
                (Some(reads), Some(reads * indexed), Some(0))
            );
            assert!(!store.prepared_disk.has_entry(&key).unwrap());
            assert!(!entry.exists());
            let rebuilt = warm(1024 * 1024);
            assert_eq!(rebuilt["status"].as_str(), Some("ok"), "{rebuilt:?}");
            assert_eq!(rebuilt["data"]["complete"].as_bool(), Some(true));
            assert_eq!(
                rebuilt["data"]["fact_cache_writes"].as_u64(),
                Some(round as u64 + 2)
            );
            assert_eq!(store.prepared_disk.entry_file_len(&key), stored);
            assert!(store.lock_state().has_prepared_facts(&identity));
        }

        evict_memory_facts(&store, &member);
        let directory = entry.parent().unwrap();
        let displaced = directory.with_extension("displaced");
        std::fs::rename(directory, &displaced).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .unwrap();
        let foreign = [warm(indexed - 1), warm(indexed)];
        std::fs::remove_dir(directory).unwrap();
        std::fs::rename(&displaced, directory).unwrap();
        for reply in &foreign {
            assert_eq!(refusal(reply), DIRECTORY_CHANGED, "{reply:?}");
            assert_eq!(cache_reads(reply), (Some(0), Some(0), Some(0)));
        }
        let restored = warm(indexed);
        assert_eq!(
            restored["data"]["complete"].as_bool(),
            Some(true),
            "{restored:?}"
        );
        assert_eq!(cache_reads(&restored), (Some(1), Some(indexed), Some(0)));
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
            json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":has_override("missing-token", &[]),"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]})
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
        let mut large_index_query = large_query.clone();
        large_index_query.insert("query", has_tool("Missing"));
        ask(&large_index_query);
        assert_eq!(resident_member_is_full(&store, &large), Some(false));
        ask(&large_query);
        ask(&graph(&small));
        assert!(store
            .state
            .lock()
            .unwrap()
            .has_prepared_facts(&identity(&large)));
        assert_eq!(resident_member_is_full(&store, &large), Some(false));
        assert!(store
            .state
            .lock()
            .unwrap()
            .has_prepared_facts(&identity(&small)));
        let writes = store.prepared_disk.stats().writes;
        let before = store.state.lock().unwrap().counters;
        ask(&large_query);
        assert_eq!(store.state.lock().unwrap().counters[1], before[1]);
        assert_eq!(store.prepared_disk.stats().writes, writes);
        assert_eq!(resident_member_is_full(&store, &large), Some(false));
        assert!(store
            .state
            .lock()
            .unwrap()
            .has_prepared_facts(&identity(&small)));
    }

    #[test]
    fn a_fact_budget_below_the_working_set_rereads_it_from_disk_on_every_pass() {
        #[derive(Debug, PartialEq)]
        struct Pass {
            value: Option<bool>,
            cache_reads: u64,
            cache_bytes: u64,
            source_bytes: u64,
        }

        let source = Source::new(&format!("{}\n", user("root")));
        let children: Vec<PathBuf> = (0..6)
            .map(|index| {
                let child = source.directory.join(format!("child-{index}.jsonl"));
                let tool = if index == 5 { "Edit" } else { "Read" };
                std::fs::write(
                    &child,
                    format!(
                        "{}\n{}\n",
                        user(&format!("child-{index}")),
                        format_args!(r#"{{"type":"assistant","uuid":"tool-{index}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"call-{index}","name":"{tool}","input":{{"file_path":"/repo/file-{index}.rs"}}}}]}}}}"#)
                    ),
                )
                .unwrap();
                child
            })
            .collect();
        let direct_paths: Vec<_> = children
            .iter()
            .map(|child| child.to_string_lossy().into_owned())
            .collect();
        let owner = context("a");
        let template = acquire(&source.path);
        let open = |fact_budget: usize| {
            let store = NativeStore::new(&json!({"max_read_bytes_per_step":8192,"max_entry_bytes":128*1024,"max_retained_bytes":32*1024*1024,"max_prepared_fact_memory_bytes":fact_budget,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
            let root = finish(
                &store,
                store.request(&acquire(&source.path), &owner, &Cancellation::default()),
                &owner,
            );
            let graph = finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":[],"roots":[],"direct_paths":direct_paths,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
            let query = json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":"Edit","subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            (store, query)
        };
        let pass = |store: &NativeStore, query: &Value| {
            let before = store.lock_state().counters;
            let reply = finish_prepared(
                store,
                store.request(query, &owner, &Cancellation::default()),
                &owner,
            );
            let after = store.lock_state().counters;
            Pass {
                value: reply["data"]["value"].as_bool(),
                cache_reads: after[18] - before[18],
                cache_bytes: after[19] - before[19],
                source_bytes: after[1] - before[1],
            }
        };

        let (store, query) = open(16 * 1024 * 1024);
        assert_eq!(pass(&store, &query).value, Some(true));
        let built = store.lock_state().ledger.shared.facts();
        let built_children: usize = {
            let state = store.lock_state();
            children
                .iter()
                .map(|child| {
                    state
                        .cached_prepared_facts(
                            &SourceStamp::of(&std::fs::metadata(child).unwrap()).identity,
                        )
                        .facts
                        .accounted_bytes()
                })
                .sum()
        };
        evict_memory_facts(&store, &source.path);
        for child in &children {
            evict_memory_facts(&store, child);
        }
        let stored: Vec<u64> = children
            .iter()
            .map(|child| index_read_bytes(&store, &fact_entry_key(child, &owner)))
            .collect();
        assert_eq!(
            pass(&store, &query),
            Pass {
                value: Some(true),
                cache_reads: children.len() as u64,
                cache_bytes: stored.iter().sum(),
                source_bytes: 0,
            }
        );
        let mut decoded: Vec<usize> = {
            let state = store.lock_state();
            children
                .iter()
                .map(|child| {
                    state
                        .cached_prepared_facts(
                            &SourceStamp::of(&std::fs::metadata(child).unwrap()).identity,
                        )
                        .facts
                        .accounted_bytes()
                })
                .collect()
        };
        decoded.sort_unstable();
        let working_set = store.lock_state().ledger.shared.facts();
        assert_eq!(
            working_set,
            built - built_children + decoded.iter().sum::<usize>()
        );
        let held_by_the_graph = working_set - decoded.iter().sum::<usize>();
        let fitting = |budget: usize| {
            decoded
                .iter()
                .scan(held_by_the_graph, |used, size| {
                    *used += size;
                    Some(*used)
                })
                .take_while(|used| *used <= budget)
                .count()
        };
        let steady = |budget: usize| {
            let (store, query) = open(budget);
            let passes: Vec<Pass> = (0..8).map(|_| pass(&store, &query)).collect();
            assert!(store.lock_state().ledger.shared.facts() <= budget);
            passes.into_iter().skip(4).collect::<Vec<_>>()
        };

        for budget in [built, working_set * 8 / 5] {
            for repeated in steady(budget) {
                assert_eq!(
                    repeated,
                    Pass {
                        value: Some(true),
                        cache_reads: 0,
                        cache_bytes: 0,
                        source_bytes: 0,
                    },
                    "budget {budget} of working set {working_set}"
                );
            }
        }
        assert_eq!(
            (fitting(working_set - 1), fitting(working_set * 2 / 5)),
            (5, 1)
        );
        for budget in [working_set - 1, working_set * 2 / 5] {
            let evicted = (children.len() - fitting(budget)) as u64;
            for repeated in steady(budget) {
                assert_eq!(
                    (repeated.value, repeated.source_bytes),
                    (Some(true), 0),
                    "budget {budget} of working set {working_set}"
                );
                assert!(
                    repeated.cache_reads >= evicted
                        && repeated.cache_bytes >= evicted * stored.iter().min().unwrap(),
                    "budget {budget} of working set {working_set}: {repeated:?}"
                );
            }
        }
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
        let retained_cap = 2 * dom_parse_bound(content.len()).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_retained_bytes":retained_cap,"max_entry_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
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
    fn registered_graph_admits_only_members_active_since_the_cutoff() {
        let tool = |name: &str| {
            format!(
                r#"{{"type":"assistant","uuid":"{name}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"{name}","name":"{name}","input":{{}}}}]}}}}"#
            )
        };
        let source = Source::new(&format!("{}\n", user("root")));
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let ids: Vec<String> = (0..1030).map(|index| format!("thread-{index}")).collect();
        for id in &ids {
            let path = source.directory.join(format!("{id}.jsonl"));
            let body = match id.as_str() {
                "thread-3" => format!("{}\n{}\n", user(id), tool("Grep")),
                "thread-7" => format!("{}\n{}\n", user(id), tool("Read")),
                _ => format!("{}\n", user(id)),
            };
            std::fs::write(&path, body).unwrap();
            if id != "thread-7" {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(stale)
                    .unwrap();
            }
        }
        let store = store();
        let owner = context("a");
        let mut background = owner.clone();
        background.insert("work_class", json!("background"));
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let mut template = acquire(&source.path);
        template["limits"].insert("max_discovery_entries", json!(20_000));
        let since = now_ms() - 60_000;
        let warm = || {
            let mut request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"active_since_unix_ms":since,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
            for _ in 0..12 {
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
            panic!("registered warming did not complete");
        };
        let prepare = json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(&root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"active_since_unix_ms":since,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        let has = |graph: &Value, name: &str| {
            let result = finish_prepared(
                &store,
                store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":name,"subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), &owner, &Cancellation::default()),
                &owner,
            );
            assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
            result["data"]["value"].as_bool().unwrap()
        };
        warm();
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert!(has(&graph, "Read"));
        assert!(!has(&graph, "Grep"));
        std::fs::OpenOptions::new()
            .append(true)
            .open(source.directory.join("thread-3.jsonl"))
            .unwrap()
            .write_all(format!("{}\n", user("resumed")).as_bytes())
            .unwrap();
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        let resumed = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(resumed["status"].as_str(), Some("ok"), "{resumed:?}");
        assert!(has(&resumed, "Grep"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
    }

    fn tool_use_line(name: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{name}","sessionId":"s","timestamp":"2026-01-02T03:04:06Z","message":{{"model":"test","content":[{{"type":"tool_use","id":"{name}","name":"{name}","input":{{}}}}]}}}}"#
        )
    }

    fn registered_prepare(source: &Source, root: &Value, ids: &[&str]) -> Value {
        let template = acquire(&source.path);
        json!({"schema":SCHEMA,"id":"prepare","operation":"prepare_graph","view":{"handle":handle(root),"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]})
    }

    fn graph_has_tool(
        store: &NativeStore,
        source: &Source,
        owner: &Value,
        graph: &Value,
        name: &str,
    ) -> bool {
        let template = acquire(&source.path);
        let result = finish_prepared(
            store,
            store.request(&json!({"schema":SCHEMA,"id":"query","operation":"query_graph","handle":graph["data"]["handle"],"selectors":[],"query":{"kind":"has_tool","pattern":name,"subagents":true},"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]}), owner, &Cancellation::default()),
            owner,
        );
        assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
        result["data"]["value"].as_bool().unwrap()
    }

    #[test]
    fn cold_registered_graph_prepares_only_its_requested_members_on_demand() {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = registered_members(&source, &["thread-a", "thread-b"]);
        std::fs::write(
            &members[0],
            format!("{}\n{}\n", user("a"), tool_use_line("Read")),
        )
        .unwrap();
        let unrelated = source.directory.join("thread-z.jsonl");
        std::fs::write(
            &unrelated,
            format!("{}\n{}\n", user("z"), tool_use_line("Grep")),
        )
        .unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prepare = registered_prepare(&source, &root, &["thread-a", "thread-b"]);
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert!(!graph_has_tool(&store, &source, &owner, &graph, "Grep"));
        assert!(graph_has_tool(&store, &source, &owner, &graph, "Read"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), 3);
        {
            let state = store.lock_state();
            assert!(state.has_prepared_facts(&member_identity(&members[0])));
            assert!(state.has_prepared_facts(&member_identity(&members[1])));
            assert!(!state.has_prepared_facts(&member_identity(&unrelated)));
        }
        let lookups = store.fact_lookups.load(Ordering::Relaxed);
        let again = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(again["status"].as_str(), Some("ok"), "{again:?}");
        assert!(graph_has_tool(&store, &source, &owner, &again, "Read"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed), 3);
        assert_eq!(store.fact_lookups.load(Ordering::Relaxed), lookups);
        assert!(!store
            .lock_state()
            .has_prepared_facts(&member_identity(&unrelated)));
        store.assert_conserved();
    }

    #[test]
    fn registered_graph_sees_appends_and_new_children_without_warming() {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = registered_members(&source, &["thread-a", "thread-b"]);
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prepare = registered_prepare(&source, &root, &["thread-a", "thread-b"]);
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert!(!graph_has_tool(&store, &source, &owner, &graph, "Grep"));
        assert!(!graph_has_tool(&store, &source, &owner, &graph, "Bash"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&members[1])
            .unwrap()
            .write_all(format!("{}\n", tool_use_line("Grep")).as_bytes())
            .unwrap();
        let appended = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(appended["status"].as_str(), Some("ok"), "{appended:?}");
        assert!(graph_has_tool(&store, &source, &owner, &appended, "Grep"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        let subagents = source.directory.join("thread-a").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("child.jsonl"),
            format!("{}\n{}\n", user("child"), tool_use_line("Bash")),
        )
        .unwrap();
        let discovered = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(discovered["status"].as_str(), Some("ok"), "{discovered:?}");
        assert!(graph_has_tool(&store, &source, &owner, &discovered, "Bash"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 2);
        let hot = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(hot["status"].as_str(), Some("ok"), "{hot:?}");
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 2);
        store.assert_conserved();
    }

    #[test]
    fn registered_graph_append_does_not_relist_an_unchanged_sidechain_directory() {
        let source = Source::new(&format!("{}\n", user("root")));
        let members = registered_members(&source, &["thread-a", "thread-b"]);
        let subagents = source.directory.join("thread-a").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("preexisting.jsonl"),
            format!("{}\n{}\n", user("preexisting"), tool_use_line("Read")),
        )
        .unwrap();
        let store = store();
        let owner = context("a");
        let root = finish(
            &store,
            store.request(&acquire(&source.path), &owner, &Cancellation::default()),
            &owner,
        );
        let prepare = registered_prepare(&source, &root, &["thread-a", "thread-b"]);
        let discovery = || store.lock_state().counters[17];
        let directory_stamp = || SourceStamp::of(&std::fs::metadata(&subagents).unwrap());
        let graph = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(graph["status"].as_str(), Some("ok"), "{graph:?}");
        assert!(graph_has_tool(&store, &source, &owner, &graph, "Read"));
        assert!(!graph_has_tool(&store, &source, &owner, &graph, "Grep"));
        assert!(!graph_has_tool(&store, &source, &owner, &graph, "Bash"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        let listed = directory_stamp();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&members[1])
            .unwrap()
            .write_all(format!("{}\n", tool_use_line("Grep")).as_bytes())
            .unwrap();
        assert_eq!(directory_stamp(), listed);
        let before = discovery();
        let appended = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(
            discovery() - before,
            0,
            "a member append listed an unchanged sidechain directory again"
        );
        assert_eq!(appended["status"].as_str(), Some("ok"), "{appended:?}");
        assert!(graph_has_tool(&store, &source, &owner, &appended, "Read"));
        assert!(graph_has_tool(&store, &source, &owner, &appended, "Grep"));
        assert!(!graph_has_tool(&store, &source, &owner, &appended, "Bash"));
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 1);
        store.assert_conserved();
        std::fs::write(
            subagents.join("child.jsonl"),
            format!("{}\n{}\n", user("child"), tool_use_line("Bash")),
        )
        .unwrap();
        assert_ne!(directory_stamp(), listed);
        let before = discovery();
        let discovered = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert!(discovery() > before);
        assert_eq!(discovered["status"].as_str(), Some("ok"), "{discovered:?}");
        for tool in ["Read", "Grep", "Bash"] {
            assert!(graph_has_tool(&store, &source, &owner, &discovered, tool));
        }
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 2);
        let before = discovery();
        let hot = finish_prepared(
            &store,
            store.request(&prepare, &owner, &Cancellation::default()),
            &owner,
        );
        assert_eq!(discovery(), before);
        assert_eq!(hot["status"].as_str(), Some("ok"), "{hot:?}");
        for tool in ["Read", "Grep", "Bash"] {
            assert!(graph_has_tool(&store, &source, &owner, &hot, tool));
        }
        assert_eq!(store.warm_records.load(Ordering::Relaxed), 2);
        store.assert_conserved();
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

    fn registered_warming(count: usize) -> (Source, NativeStore, Value, Value) {
        let source = Source::new(&format!("{}\n", user("root")));
        let ids: Vec<_> = (0..count)
            .map(|index| format!("thread-{index:04}"))
            .collect();
        for id in &ids {
            std::fs::write(
                source.directory.join(format!("{id}.jsonl")),
                format!("{}\n", user(id)),
            )
            .unwrap();
        }
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":4096,"max_events_per_step":2048,"max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let mut owner = context("a");
        owner.insert("work_class", json!("background"));
        let mut template = acquire(&source.path);
        template["limits"].insert("max_discovery_entries", json!(20_000));
        template["limits"].insert("max_sources", json!(1024));
        let request = json!({"schema":SCHEMA,"id":"warm","operation":"warm_registered","classifier":{"id":"native","version":"1"},"thread_ids":ids,"roots":[source.directory.to_string_lossy().as_ref()],"direct_paths":[],"start_index":0,"membership_revision":null,"deadline_unix_ms":template["deadline_unix_ms"],"limits":template["limits"]});
        (source, store, owner, request)
    }

    fn registered_pages(store: &NativeStore, owner: &Value, request: &Value) -> Vec<Value> {
        let mut request = request.clone();
        let mut pages = Vec::new();
        for _ in 0..128 {
            request.insert("deadline_unix_ms", json!(now_ms() + 120_000));
            let reply = store.request(&request, owner, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            request.insert("start_index", reply["data"]["next_index"].clone());
            request.insert(
                "membership_revision",
                reply["data"]["membership_revision"].clone(),
            );
            let complete = reply["data"]["complete"].as_bool() == Some(true);
            pages.push(reply);
            if complete {
                return pages;
            }
        }
        panic!("registered warming did not complete");
    }

    fn page_ends(pages: &[Value]) -> Vec<(u64, bool)> {
        pages
            .iter()
            .map(|page| {
                (
                    page["data"]["next_index"].as_u64().unwrap(),
                    page["data"]["complete"].as_bool().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn registered_warmer_pages_cached_members_256_at_a_time() {
        let (_source, store, owner, request) = registered_warming(600);
        let cold = registered_pages(&store, &owner, &request);
        assert_eq!(
            page_ends(&cold),
            (1..=75u64)
                .map(|page| (page * 8, page == 75))
                .collect::<Vec<_>>()
        );
        let revision = cold[0]["data"]["membership_revision"].as_str().unwrap();
        let resolutions = store.source_resolutions.load(Ordering::Relaxed);
        let dir_checks = store.prepared_disk.dir_checks.load(Ordering::Relaxed);
        let metadata_checks = store.membership_metadata_checks.load(Ordering::Relaxed);
        let writes = store.prepared_disk.stats().writes;
        let cached = registered_pages(&store, &owner, &request);
        assert_eq!(
            page_ends(&cached),
            [(256, false), (512, false), (600, true)]
        );
        for page in cold.iter().chain(&cached) {
            assert_eq!(page["data"]["membership_revision"].as_str(), Some(revision));
        }
        for page in &cached {
            assert_eq!(cache_reads(page), (Some(0), Some(0), Some(0)), "{page:?}");
            assert_eq!(page["usage"]["events_parsed"].as_u64(), Some(0), "{page:?}");
        }
        assert_eq!(
            store.source_resolutions.load(Ordering::Relaxed) - resolutions,
            10
        );
        assert_eq!(
            store.prepared_disk.dir_checks.load(Ordering::Relaxed) - dir_checks,
            2
        );
        assert_eq!(
            store.membership_metadata_checks.load(Ordering::Relaxed) - metadata_checks,
            4 * 600
        );
        assert_eq!(store.prepared_disk.stats().writes, writes);
    }

    #[test]
    fn registered_append_refresh_reserves_the_member_paths_it_copies() {
        let (source, store, owner, request) = registered_warming(20);
        let store = Arc::new(store);
        registered_pages(&store, &owner, &request);
        let warmed = store
            .lock_state()
            .warm_memberships
            .values()
            .next()
            .unwrap()
            .clone();
        let live = source_ref_bytes(&warmed.members)
            + arc_slice_bytes::<(SourceIdentity, usize)>(warmed.positions.len())
            + sidechain_dir_bytes(&warmed.sidechain_dirs)
            + warmed.members.len() * size_of::<PreparedSourceRef>();
        let copied: usize = warmed.members[..warmed.members.len() - 1]
            .iter()
            .map(|member| member.path.as_os_str().len())
            .sum();
        std::fs::OpenOptions::new()
            .append(true)
            .open(source.directory.join("thread-0000.jsonl"))
            .unwrap()
            .write_all(format!("{}\n", user("appended")).as_bytes())
            .unwrap();
        let resolved = Arc::new(Mutex::new(Vec::new()));
        *store.resolve_hook.lock().unwrap() = Some(Arc::new({
            let (store, resolved) = (Arc::downgrade(&store), Arc::clone(&resolved));
            move |path: &Path| {
                let store = store.upgrade().unwrap();
                let state = store.lock_state();
                resolved.lock().unwrap().push((
                    path.to_path_buf(),
                    state.transient_bytes,
                    state.warm_memberships.is_empty(),
                ));
            }
        }));
        let mut refresh = request.clone();
        refresh.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let reply = store.request(&refresh, &owner, &Cancellation::default());
        *store.resolve_hook.lock().unwrap() = None;
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        let resolutions = resolved.lock().unwrap();
        let uncached: Vec<_> = resolutions
            .iter()
            .filter(|(.., uncached)| *uncached)
            .collect();
        assert_eq!(
            uncached.iter().map(|(path, ..)| path).collect::<Vec<_>>(),
            warmed
                .members
                .iter()
                .map(|member| &member.path)
                .collect::<Vec<_>>()
        );
        assert!(!resolutions[0].2);
        assert!(
            uncached.last().unwrap().1 - resolutions[0].1 >= live + copied,
            "the refresh holds copied member paths its reservation never charged"
        );
        assert!(Arc::ptr_eq(
            &store
                .lock_state()
                .warm_memberships
                .values()
                .next()
                .unwrap()
                .positions,
            &warmed.positions
        ));
        store.assert_conserved();
    }

    #[test]
    fn registered_completion_rejects_an_owner_directory_replaced_between_members() {
        use std::os::unix::fs::DirBuilderExt;

        let (source, store, owner, request) = registered_warming(2);
        registered_pages(&store, &owner, &request);
        let member = std::fs::canonicalize(source.directory.join("thread-0000.jsonl")).unwrap();
        let dir = store
            .prepared_disk
            .entry_file(&fact_entry_key(&member, &owner))
            .parent()
            .unwrap()
            .to_path_buf();
        let displaced = dir.with_extension("displaced");
        let listed = Arc::new(AtomicUsize::new(0));
        *store.prepared_disk.list_hook.lock().unwrap() = Some(Arc::new({
            let (dir, displaced, listed) = (dir.clone(), displaced.clone(), Arc::clone(&listed));
            move || {
                if listed.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::fs::rename(&dir, &displaced).unwrap();
                    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
                }
            }
        }));
        let checks = store.prepared_disk.dir_checks.load(Ordering::Relaxed);
        let mut repeat = request.clone();
        repeat.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let reply = store.request(&repeat, &owner, &Cancellation::default());
        *store.prepared_disk.list_hook.lock().unwrap() = None;
        std::fs::remove_dir(&dir).unwrap();
        std::fs::rename(&displaced, &dir).unwrap();
        assert_eq!(listed.load(Ordering::SeqCst), 2);
        assert_eq!(
            store.prepared_disk.dir_checks.load(Ordering::Relaxed) - checks,
            2
        );
        assert_eq!(
            (reply["status"].as_str(), reply["reason"].as_str()),
            (
                Some("incomplete"),
                Some("prepared facts cache directory changed")
            ),
            "{reply:?}"
        );
    }

    #[test]
    fn restricted_warming_step_refuses_a_member_swapped_for_an_outside_symlink() {
        let (source, store, mut owner, request) = registered_warming(2);
        owner.insert(
            "authority",
            json!({"kind":"restricted_roots","roots":[source.directory.to_string_lossy().as_ref()],"effective_uid":unsafe { libc::geteuid() }.to_string()}),
        );
        let cold = registered_pages(&store, &owner, &request);
        let outside = Source::new(&format!("{}\n", user("outside")));
        let member = source.directory.join("thread-0001.jsonl");
        std::fs::remove_file(&member).unwrap();
        std::os::unix::fs::symlink(&outside.path, &member).unwrap();
        let mut page = request.clone();
        page.insert("start_index", json!(1));
        page.insert(
            "membership_revision",
            cold[0]["data"]["membership_revision"].clone(),
        );
        page.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let refused = store.request(&page, &owner, &Cancellation::default());
        assert_eq!(
            refused["status"].as_str(),
            Some("permission_denied"),
            "{refused:?}"
        );
    }

    #[test]
    fn registered_warming_page_rejects_a_parent_retargeted_between_members() {
        let (source, store, owner, request) = registered_warming(300);
        let cold = registered_pages(&store, &owner, &request);
        let mut page = request.clone();
        page.insert("start_index", json!(1));
        page.insert(
            "membership_revision",
            cold[0]["data"]["membership_revision"].clone(),
        );
        page.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let unchanged = store.request(&page, &owner, &Cancellation::default());
        assert_eq!(unchanged["status"].as_str(), Some("ok"), "{unchanged:?}");
        assert_eq!(unchanged["data"]["complete"].as_bool(), Some(false));
        let directory = std::fs::canonicalize(&source.directory).unwrap();
        let moved = directory.with_extension("moved");
        retarget_on_resolve(
            &store,
            &directory.join("thread-0001.jsonl"),
            &directory,
            &moved,
        );
        page.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let retargeted = store.request(&page, &owner, &Cancellation::default());
        std::fs::remove_file(&directory).unwrap();
        std::fs::rename(&moved, &directory).unwrap();
        assert_eq!(
            retargeted["status"].as_str(),
            Some("changed"),
            "{retargeted:?}"
        );
    }

    #[test]
    fn registered_warmer_stops_a_cached_page_at_eight_uncached_members() {
        let (source, store, owner, request) = registered_warming(300);
        let warmed = registered_pages(&store, &owner, &request);
        let warmed_revision = warmed[0]["data"]["membership_revision"].as_str();
        for index in 100..112 {
            std::fs::OpenOptions::new()
                .append(true)
                .open(source.directory.join(format!("thread-{index:04}.jsonl")))
                .unwrap()
                .write_all(format!("{}\n", user("appended")).as_bytes())
                .unwrap();
        }
        let builds = store.fact_builds.load(Ordering::Relaxed);
        let pages = registered_pages(&store, &owner, &request);
        assert_eq!(page_ends(&pages), [(108, false), (300, true)]);
        assert_eq!(store.fact_builds.load(Ordering::Relaxed) - builds, 12);
        let revision = pages[0]["data"]["membership_revision"].as_str();
        assert_ne!(revision, warmed_revision);
        assert_eq!(pages[1]["data"]["membership_revision"].as_str(), revision);
    }

    #[test]
    fn registered_warmer_returns_its_partial_page_when_the_deadline_passes() {
        let (_source, store, owner, mut request) = registered_warming(3);
        let deadline = now_ms() + 1_000;
        *store.built_facts_hook.lock().unwrap() = Some(Arc::new(
            move |_: &Arc<crate::snapshot_prepared::PreparedFacts>| {
                while now_ms() <= deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            },
        ));
        request.insert("deadline_unix_ms", json!(deadline));
        let partial = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(partial["status"].as_str(), Some("ok"), "{partial:?}");
        assert_eq!(page_ends(std::slice::from_ref(&partial)), [(1, false)]);
        request.insert("start_index", json!(1));
        request.insert(
            "membership_revision",
            partial["data"]["membership_revision"].clone(),
        );
        request.insert("deadline_unix_ms", json!(now_ms()));
        let stalled = store.request(&request, &owner, &Cancellation::default());
        assert_eq!(stalled["status"].as_str(), Some("deadline"), "{stalled:?}");
        assert_eq!(
            page_ends(&registered_pages(&store, &owner, &request)),
            [(3, true)]
        );
    }

    #[test]
    fn registered_warmer_cancelled_on_its_last_step_fails_its_final_hold() {
        let (source, store, owner, mut request) = registered_warming(300);
        registered_pages(&store, &owner, &request);
        std::fs::OpenOptions::new()
            .append(true)
            .open(source.directory.join("thread-0255.jsonl"))
            .unwrap()
            .write_all(format!("{}\n", user("appended")).as_bytes())
            .unwrap();
        let cancel = Cancellation::default();
        *store.built_facts_hook.lock().unwrap() = Some(Arc::new({
            let cancel = cancel.clone();
            move |_: &Arc<crate::snapshot_prepared::PreparedFacts>| cancel.cancel()
        }));
        let builds = store.fact_builds.load(Ordering::Relaxed);
        request.insert("deadline_unix_ms", json!(now_ms() + 120_000));
        let cancelled = store.request(&request, &owner, &cancel);
        assert_eq!(
            cancelled["status"].as_str(),
            Some("cancelled"),
            "{cancelled:?}"
        );
        assert_eq!(store.fact_builds.load(Ordering::Relaxed) - builds, 1);
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
        store
            .track_delivery(&json!({"cursor":cursor}), &owner, false)
            .unwrap();
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
        assert!(store.retained_accounted_bytes() > 0);
        assert_eq!(
            store
                .reserve_projection(&review, 0)
                .err()
                .expect("the startup footprint exceeds the background cap")
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
        let rows_charge = snapshot.chunks[0].rows_charge();
        let rows_bytes = rows_charge.owned_capacity_bytes + rows_charge.opaque_dom_accounted_bytes;
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
        store.lock_state().clear_classified();
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
        let mut state = store.lock_state();
        let slot = Arc::clone(state.loads.values().next().expect("published load slot"));
        assert_eq!(
            (state.loads.len(), slot.accounted.load(Ordering::Acquire)),
            (1, 0),
            "the published load slot still carries load charges"
        );
        assert_eq!(
            state.ledger.pending,
            NativeStore::audit_load_record_bytes(&slot),
            "the published load slot is not charged its record alone"
        );
        drop(slot);
        NativeStore::prune(&mut state);
        assert!(state.loads.is_empty());
        assert_eq!(state.ledger.pending, 0);
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

    fn direct_build_peak(
        store: &NativeStore,
        prepared: &Value,
        owner: &Value,
        request: &Value,
    ) -> usize {
        let state = store.lock_state();
        let graph_id = prepared["data"]["handle"]["graph_id"].as_str().unwrap();
        let key = state
            .prepared_graphs
            .iter()
            .find(|(key, _)| key.as_str() == graph_id)
            .map(|(key, _)| key.capacity())
            .unwrap();
        let graph = state.prepared_graphs[graph_id].lock().unwrap();
        assert!(state.has_prepared_facts(&graph.root.stamp.identity));
        let mut tasks = Vec::<GraphTask>::new();
        tasks.reserve(1);
        let mut dirs = Vec::<(PathBuf, Option<SourceStamp>)>::new();
        dirs.push((PathBuf::new(), None));
        size_of::<PreparedBuild>()
            + graph.claimant.capacity()
            + value_bytes(&owner.clone())
            + value_bytes(&request.clone())
            + value_bytes(&graph.root_handle)
            + value_bytes(&graph.classifier)
            + HashSet::from([graph.root.stamp.identity.file()]).capacity()
                * size_of::<SourceIdentity>()
            + graph.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
            + graph
                .stamps
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + tasks.capacity() * size_of::<GraphTask>()
            + graph.root.canonical_path.capacity()
            + FILESYSTEM_PATH_BYTES
            + dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + graph
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + key
            + arc_mirror::<Mutex<PreparedGraph>>()
            + MUTEX_STORAGE_MIRROR
            + graph.registry_generation.capacity()
            + graph.admission.capacity()
            + value_bytes(&graph.authority)
            + graph.revision.capacity()
            + arc_slice_mirror::<PreparedSourceRef>(graph.sources.len())
            + arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(graph.sidechain_dirs.len())
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
            [("fresh".to_owned(), PathBuf::from("/locations/fresh.jsonl"))]
                .iter()
                .map(|(id, path)| (id, path)),
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
            let build_bytes = direct_build_peak(&store, &measured, &owner, &prepare);
            assert!(
                build_bytes > graph_bytes,
                "the build's buffers are not admitted beyond its graph"
            );
            let anchors: Vec<_> = {
                let state = store.lock_state();
                let graph_id = measured["data"]["handle"]["graph_id"].as_str().unwrap();
                let graph = state.prepared_graphs[graph_id].lock().unwrap();
                graph.anchors().collect()
            };
            release_graph(&store, &measured, &owner);
            assert_eq!(store.retained_accounted_bytes(), idle);
            let table_growth = store.lock_state().ledger.shared.growth(anchors);
            let room = cap - idle - build_bytes - table_growth - REPLY_RESERVATION;
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
            assert_eq!(
                store.retained_accounted_bytes(),
                cap - REPLY_RESERVATION - (build_bytes - graph_bytes)
            );
            store.assert_conserved();
            release_graph(&store, &exact, &owner);
            drop(fitted);
            assert_eq!(store.retained_accounted_bytes(), idle + table_growth);
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
            let root = Arc::clone(
                &store.lock_state().prepared_graphs
                    [prepared["data"]["handle"]["graph_id"].as_str().unwrap()]
                .lock()
                .unwrap()
                .root,
            );
            let bound = slice_bytes.max(crate::snapshot_projection::facts_bound(
                &root,
                FactsNeed::Index,
            ));
            let query = slice_query(&prepared);
            let record = size_of::<PreparedQueryCursor>()
                + owner["claimant"].as_str().unwrap().len()
                + query["handle"]["graph_id"].as_str().unwrap().len()
                + NativeStore::audit_value_bytes(&query["query"]);
            let need = bound.max(slice_bytes + record);
            let room = cap - idle - need - REPLY_RESERVATION;
            let crowded = store
                .reserve_projection(&owner, room + need - bound + 1)
                .unwrap();
            let refused = store.request(&query, &owner, &Cancellation::default());
            assert_refused(&store, &refused);
            assert_eq!(
                store.retained_accounted_bytes(),
                idle + room + need - bound + 1
            );
            assert_eq!(slices(&prepared), 0);
            drop(crowded);
            if need > bound {
                let crowded = store.reserve_projection(&owner, room + 1).unwrap();
                let refused = store.request(&query, &owner, &Cancellation::default());
                assert_refused(&store, &refused);
                assert_eq!(slices(&prepared), 1);
                assert_eq!(
                    settled_bytes(&store),
                    idle - capacities + room + 1 + slice_bytes
                );
                drop(crowded);
            }
            let fitted = store.reserve_projection(&owner, room).unwrap();
            let exact = store.request(&query, &owner, &Cancellation::default());
            assert_eq!(exact["status"].as_str(), Some("ok"), "{exact:?}");
            assert_eq!(
                settled_bytes(&store),
                cap - REPLY_RESERVATION - capacities - (need - slice_bytes)
            );
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

    #[test]
    fn record_transport_completes_while_the_store_state_is_held() {
        let store = Arc::new(store());
        let held = store.state.lock().unwrap();
        let (finished, completion) = std::sync::mpsc::channel();
        let recorder = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store.record_transport(4096);
                store.record_transport(64);
                finished.send(()).unwrap();
            })
        };
        let outcome = completion.recv_timeout(Duration::from_secs(10));
        drop(held);
        recorder.join().unwrap();
        outcome.expect("record_transport blocked behind the held store state");
        let stats = store.request(
            &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
            &context("owner"),
            &Cancellation::default(),
        );
        assert_eq!(
            stats["data"]["counters"]["transport_bytes"].as_u64(),
            Some(4160)
        );
    }

    #[test]
    fn transport_bytes_are_conserved_across_usage_merges() {
        let store = store();
        let owner = context("owner");
        let stats = || {
            let reply = store.request(
                &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
                &owner,
                &Cancellation::default(),
            );
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            let mut counters = [0u64; 20];
            for (slot, name) in counters.iter_mut().zip(COUNTERS.iter()) {
                *slot = reply["data"]["counters"][*name].as_u64().unwrap();
            }
            counters
        };
        let before = stats();
        store.record_transport(100);
        let locked = store.state.lock().unwrap().counters;
        store.record_transport(28);
        assert_eq!(store.state.lock().unwrap().counters, locked);
        let after = stats();
        assert_eq!(after[14] - before[14], 128);
        assert_eq!(store.state.lock().unwrap().counters[14], 0);
        for slot in (0..20).filter(|slot| *slot != 13 && *slot != 14) {
            assert_eq!(after[slot], before[slot], "{}", COUNTERS[slot]);
        }
        let merged = stats();
        assert_eq!(merged[14] - before[14], 128);
        assert_eq!(store.state.lock().unwrap().counters[14], 0);
    }
    include!("snapshot_spine_tests.rs");
    include!("snapshot_cache_tests.rs");
    include!("snapshot_source_cache_index_tests.rs");

    include!("snapshot_load_charge_work_tests.rs");
    include!("snapshot_load_charge_audit_tests.rs");
}

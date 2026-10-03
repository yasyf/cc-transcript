use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::mem::size_of;
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use regex_syntax::hir::literal::{ExtractKind, Extractor};
use regex_syntax::ParserBuilder;
use serde::{Deserialize, Serialize};

use crate::scan::{ScanBudget, StagingReservation};
use crate::scan_checkpoint::digest;
use crate::scan_projection::{grams, Projected};
use crate::scan_stream::LineSpan;
use crate::snapshot::{Cancellation, SnapshotError, Status};
use crate::types::Entry;

pub const MIN_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const BLOCK_BYTES: usize = 32 * 1024;
const BLOCK_EVENTS: usize = 256;
const SEGMENT_BLOCKS: usize = 64;
const CHUNK_KEYS: usize = 64;
const DENSE: usize = 8;
const ROW_BYTES: usize = 20;
const MAGIC: &[u8; 4] = b"CXS1";
const HEAD_BYTES: usize = 16;
const CHUNK_BYTES: usize = 16;

pub type Need = Option<Vec<Vec<u32>>>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub first: usize,
    pub offset: u64,
    pub digest: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SegmentRef {
    pub name: String,
    pub first: usize,
    pub blocks: usize,
    pub len: u64,
    pub head: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IndexLayer {
    pub generation: String,
    pub committed: u64,
    pub fence: Vec<u8>,
    pub sniffed: bool,
    pub events: usize,
    pub proj: u64,
    pub blocks: Vec<BlockRef>,
    pub open: BlockRef,
    pub segments: Vec<SegmentRef>,
}

struct Chunk {
    first: u32,
    offset: usize,
    digest: u64,
}

struct Segment {
    file: File,
    blocks: usize,
    payload: usize,
    len: usize,
    chunks: Vec<Chunk>,
}

pub enum Opened {
    Ready(IndexReader),
    Missing,
    Damaged,
}

enum Fault {
    Missing,
    Damaged,
}

pub struct IndexReader {
    pub layer: IndexLayer,
    proj: File,
    rows: File,
    candidates: Vec<bool>,
    damaged: Cell<bool>,
}

struct OpenBlock {
    first: usize,
    offset: u64,
    bytes: Vec<u8>,
    keys: Vec<u32>,
    events: usize,
    loaded: bool,
}

pub struct Builder<'store> {
    dir: PathBuf,
    generation: String,
    proj: File,
    rows: File,
    proj_len: u64,
    rows_written: usize,
    pending: Vec<u8>,
    events: usize,
    committed: u64,
    fence: Vec<u8>,
    sniffed: bool,
    cap: u64,
    blocks: Vec<BlockRef>,
    segments: Vec<SegmentRef>,
    tail: Option<SegmentRef>,
    postings: Option<BTreeMap<u32, u64>>,
    dirty: bool,
    open: OpenBlock,
    previous: BlockRef,
    uncommitted: Option<(usize, usize, usize)>,
    retired: Vec<String>,
    full: bool,
    broken: bool,
    damaged: bool,
    open_staging: StagingReservation<'store>,
    postings_staging: StagingReservation<'store>,
}

fn damaged() -> SnapshotError {
    SnapshotError::new(Status::Changed, "grep index changed")
}

fn nonce() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "{:016x}",
        digest(
            format!(
                "{} {} {:?}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
            )
            .as_bytes()
        )
    )
}

pub fn open_private(path: &Path, write: bool) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(write)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("index file is not private"));
    }
    Ok(file)
}

pub fn needs(pattern: &str, ignore_case: bool) -> [Need; 2] {
    let hir = ParserBuilder::new()
        .case_insensitive(ignore_case)
        .nest_limit(64)
        .build()
        .parse(pattern)
        .expect("compiled patterns parse");
    [ExtractKind::Prefix, ExtractKind::Suffix].map(|kind| {
        Extractor::new()
            .kind(kind)
            .extract(&hir)
            .literals()?
            .iter()
            .map(|literal| {
                let mut keys: Vec<u32> = grams(literal.as_bytes()).collect();
                keys.sort_unstable();
                keys.dedup();
                (!keys.is_empty()).then_some(keys)
            })
            .collect()
    })
}

fn mask(need: &Need, bits: &HashMap<u32, u64>, full: u64) -> u64 {
    need.as_ref().map_or(full, |literals| {
        literals.iter().fold(0, |any, keys| {
            any | keys
                .iter()
                .fold(full, |all, key| all & bits.get(key).copied().unwrap_or(0))
        })
    })
}

fn full(blocks: usize) -> u64 {
    match blocks {
        SEGMENT_BLOCKS => u64::MAX,
        _ => (1u64 << blocks) - 1,
    }
}

fn read_at(file: &File, offset: u64, len: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0; len];
    file.read_exact_at(&mut bytes, offset).ok()?;
    Some(bytes)
}

fn word(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize
}

fn parse_chunk(bytes: &[u8], blocks: usize) -> Option<Vec<(u32, u64)>> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let key = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        let count = *bytes.get(at + 4)? as usize;
        at += 5;
        let bits = match count {
            0 => {
                let bits = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
                at += 8;
                bits
            }
            _ => {
                let ids = bytes.get(at..at + count)?;
                at += count;
                ids.iter().try_fold(0u64, |bits, id| {
                    ((*id as usize) < blocks).then(|| bits | 1u64 << *id)
                })?
            }
        };
        entries.push((key, bits));
    }
    Some(entries)
}

fn encode_segment(postings: &BTreeMap<u32, u64>, blocks: usize) -> (Vec<u8>, u64) {
    let mut payload = Vec::new();
    let mut chunks = Vec::new();
    let entries: Vec<_> = postings.iter().collect();
    for chunk in entries.chunks(CHUNK_KEYS) {
        let start = payload.len();
        for (key, bits) in chunk {
            payload.extend_from_slice(&key.to_be_bytes());
            match bits.count_ones() as usize {
                count if count >= DENSE => {
                    payload.push(0);
                    payload.extend_from_slice(&bits.to_le_bytes());
                }
                count => {
                    payload.push(count as u8);
                    payload.extend((0..64u8).filter(|id| **bits & 1u64 << *id != 0));
                }
            }
        }
        chunks.push((*chunk[0].0, start, digest(&payload[start..])));
    }
    let mut bytes = Vec::with_capacity(HEAD_BYTES + chunks.len() * CHUNK_BYTES + payload.len());
    bytes.extend_from_slice(MAGIC);
    for value in [blocks, postings.len(), chunks.len()] {
        bytes.extend_from_slice(&(value as u32).to_le_bytes());
    }
    for (first, offset, sum) in &chunks {
        bytes.extend_from_slice(&first.to_le_bytes());
        bytes.extend_from_slice(&(*offset as u32).to_le_bytes());
        bytes.extend_from_slice(&sum.to_le_bytes());
    }
    let head = digest(&bytes);
    bytes.extend_from_slice(&payload);
    (bytes, head)
}

impl Segment {
    fn open(
        dir: &Path,
        segment: &SegmentRef,
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<Result<Self, Fault>, SnapshotError> {
        let file = match open_private(&dir.join(&segment.name), false) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Err(Fault::Missing)),
            Err(_) => return Ok(Err(Fault::Damaged)),
        };
        budget.charge_projection(HEAD_BYTES, 0, cancel)?;
        let Some(head) = read_at(&file, 0, HEAD_BYTES).filter(|head| &head[..4] == MAGIC) else {
            return Ok(Err(Fault::Damaged));
        };
        let count = word(&head, 12);
        let payload = HEAD_BYTES + count * CHUNK_BYTES;
        if word(&head, 4) != segment.blocks
            || (segment.len as usize) < payload
            || file
                .metadata()
                .map_or(true, |metadata| metadata.len() < segment.len)
        {
            return Ok(Err(Fault::Damaged));
        }
        budget.charge_projection(count * CHUNK_BYTES, 0, cancel)?;
        let Some(table) = read_at(&file, HEAD_BYTES as u64, count * CHUNK_BYTES)
            .filter(|table| digest(&[head.as_slice(), table.as_slice()].concat()) == segment.head)
        else {
            return Ok(Err(Fault::Damaged));
        };
        let chunks = table
            .chunks(CHUNK_BYTES)
            .map(|entry| Chunk {
                first: word(entry, 0) as u32,
                offset: word(entry, 4),
                digest: u64::from_le_bytes(entry[8..].try_into().expect("eight bytes")),
            })
            .collect();
        Ok(Ok(Self {
            file,
            blocks: segment.blocks,
            payload,
            len: segment.len as usize,
            chunks,
        }))
    }

    fn chunk(
        &self,
        index: usize,
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<Option<Vec<(u32, u64)>>, SnapshotError> {
        let start = self.payload + self.chunks[index].offset;
        let end = self
            .chunks
            .get(index + 1)
            .map_or(self.len, |next| self.payload + next.offset);
        if end < start || end > self.len {
            return Ok(None);
        }
        budget.charge_projection(end - start, 0, cancel)?;
        Ok(read_at(&self.file, start as u64, end - start)
            .filter(|bytes| digest(bytes) == self.chunks[index].digest)
            .and_then(|bytes| parse_chunk(&bytes, self.blocks)))
    }

    fn lookup(
        &self,
        keys: &[u32],
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<Option<HashMap<u32, u64>>, SnapshotError> {
        let mut bits = HashMap::new();
        let mut cached: Option<(usize, Vec<(u32, u64)>)> = None;
        for key in keys {
            let at = self.chunks.partition_point(|chunk| chunk.first <= *key);
            if at == 0 {
                continue;
            }
            if cached.as_ref().is_none_or(|(index, _)| *index != at - 1) {
                let Some(entries) = self.chunk(at - 1, budget, cancel)? else {
                    return Ok(None);
                };
                cached = Some((at - 1, entries));
            }
            let (_, entries) = cached.as_ref().expect("chunk cached");
            if let Ok(found) = entries.binary_search_by_key(key, |(key, _)| *key) {
                bits.insert(*key, entries[found].1);
            }
        }
        Ok(Some(bits))
    }

    fn decode(
        &self,
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<Option<BTreeMap<u32, u64>>, SnapshotError> {
        let mut postings = BTreeMap::new();
        for index in 0..self.chunks.len() {
            let Some(entries) = self.chunk(index, budget, cancel)? else {
                return Ok(None);
            };
            postings.extend(entries);
        }
        Ok(Some(postings))
    }
}

impl IndexLayer {
    fn coherent(&self) -> bool {
        let starts: Vec<&BlockRef> = self.blocks.iter().chain([&self.open]).collect();
        let tiled = self
            .segments
            .iter()
            .enumerate()
            .try_fold(0, |at, (position, segment)| {
                (segment.first == at
                    && (1..=SEGMENT_BLOCKS).contains(&segment.blocks)
                    && (segment.blocks == SEGMENT_BLOCKS || position + 1 == self.segments.len()))
                .then_some(at + segment.blocks)
            });
        starts[0].first == 0
            && starts[0].offset == 0
            && starts
                .windows(2)
                .all(|pair| pair[0].first <= pair[1].first && pair[0].offset <= pair[1].offset)
            && self.open.first <= self.events
            && self.open.offset <= self.proj
            && tiled == Some(self.blocks.len())
    }

    fn block_range(&self, block: usize) -> (Range<usize>, Range<u64>, u64) {
        match self.blocks.get(block) {
            Some(start) => {
                let next = self.blocks.get(block + 1).unwrap_or(&self.open);
                (
                    start.first..next.first,
                    start.offset..next.offset,
                    start.digest,
                )
            }
            None => (
                self.open.first..self.events,
                self.open.offset..self.proj,
                self.open.digest,
            ),
        }
    }
}

impl IndexReader {
    pub fn open(
        dir: &Path,
        layer: IndexLayer,
        needs: &[[Need; 2]],
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<Opened, SnapshotError> {
        let files = ["proj", "events"]
            .map(|kind| open_private(&dir.join(format!("{kind}-{}", layer.generation)), false));
        let [proj, rows] = match files {
            [Ok(proj), Ok(rows)] => [proj, rows],
            [Err(error), _] | [_, Err(error)] if error.kind() == ErrorKind::NotFound => {
                return Ok(Opened::Missing)
            }
            _ => return Ok(Opened::Damaged),
        };
        if !layer.coherent()
            || proj
                .metadata()
                .map_or(true, |metadata| metadata.len() < layer.proj)
            || rows.metadata().map_or(true, |metadata| {
                metadata.len() < (layer.events as u64).saturating_mul(ROW_BYTES as u64)
            })
        {
            return Ok(Opened::Damaged);
        }
        let mut candidates = vec![true; layer.blocks.len()];
        if needs
            .iter()
            .all(|[prefix, suffix]| prefix.is_some() || suffix.is_some())
        {
            let keys: Vec<u32> = needs
                .iter()
                .flatten()
                .flatten()
                .flatten()
                .flatten()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            for reference in &layer.segments {
                let segment = match Segment::open(dir, reference, budget, cancel)? {
                    Ok(segment) => segment,
                    Err(Fault::Missing) => return Ok(Opened::Missing),
                    Err(Fault::Damaged) => return Ok(Opened::Damaged),
                };
                let Some(bits) = segment.lookup(&keys, budget, cancel)? else {
                    return Ok(Opened::Damaged);
                };
                let full = full(reference.blocks);
                let hits = needs.iter().fold(0, |any, [prefix, suffix]| {
                    any | mask(prefix, &bits, full) & mask(suffix, &bits, full)
                });
                for (offset, candidate) in candidates
                    [reference.first..reference.first + reference.blocks]
                    .iter_mut()
                    .enumerate()
                {
                    *candidate = hits & 1u64 << offset != 0;
                }
            }
        }
        Ok(Opened::Ready(Self {
            layer,
            proj,
            rows,
            candidates,
            damaged: Cell::new(false),
        }))
    }

    pub fn blocks(&self) -> usize {
        self.layer.blocks.len() + 1
    }

    pub fn events(&self, block: usize) -> Range<usize> {
        self.layer.block_range(block).0
    }

    pub fn candidate(&self, block: usize) -> bool {
        self.candidates.get(block).copied().unwrap_or(true)
    }

    pub fn is_damaged(&self) -> bool {
        self.damaged.get()
    }

    pub fn damage(&self) -> SnapshotError {
        self.damaged.set(true);
        damaged()
    }

    pub fn read<'store>(
        &self,
        block: usize,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(Vec<Projected>, StagingReservation<'store>), SnapshotError> {
        let (events, bytes, sum) = self.layer.block_range(block);
        let len = (bytes.end - bytes.start) as usize;
        budget.charge_projection(len, 0, cancel)?;
        let staging = budget.reserve_staging(len.saturating_mul(2), cancel)?;
        let projected = read_at(&self.proj, bytes.start, len)
            .filter(|bytes| digest(bytes) == sum)
            .and_then(|bytes| Projected::decode_all(&bytes))
            .filter(|projected| projected.len() == events.len())
            .ok_or_else(|| self.damage())?;
        Ok((projected, staging))
    }

    pub fn row(
        &self,
        index: usize,
        budget: &mut ScanBudget<'_>,
        cancel: &Cancellation,
    ) -> Result<(LineSpan, u64), SnapshotError> {
        budget.charge_projection(ROW_BYTES, 0, cancel)?;
        let bytes = (index < self.layer.events)
            .then(|| read_at(&self.rows, (index * ROW_BYTES) as u64, ROW_BYTES))
            .flatten()
            .ok_or_else(|| self.damage())?;
        Ok((
            LineSpan {
                offset: u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes")),
                len: word(&bytes, 8),
                terminated: true,
            },
            u64::from_le_bytes(bytes[12..].try_into().expect("eight bytes")),
        ))
    }
}

impl<'store> Builder<'store> {
    pub fn start(
        dir: &Path,
        layer: Option<&IndexLayer>,
        cap: u64,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Option<Self>, SnapshotError> {
        let generation = layer.map_or_else(nonce, |layer| layer.generation.clone());
        let keep: HashSet<String> = layer
            .into_iter()
            .flat_map(|layer| layer.segments.iter().map(|segment| segment.name.clone()))
            .chain(["proj", "events"].map(|kind| format!("{kind}-{generation}")))
            .collect();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.take(4096).flatten() {
                if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let files = ["proj", "events"]
            .map(|kind| open_private(&dir.join(format!("{kind}-{generation}")), true));
        let [Ok(proj), Ok(rows)] = files else {
            return Ok(None);
        };
        let (proj_len, events) = layer.map_or((0, 0), |layer| (layer.proj, layer.events));
        let fits = |file: &File, len: u64| {
            file.metadata().is_ok_and(|metadata| metadata.len() >= len) && file.set_len(len).is_ok()
        };
        if !fits(&proj, proj_len) || !fits(&rows, (events * ROW_BYTES) as u64) {
            return Ok(None);
        }
        let empty = BlockRef {
            first: 0,
            offset: 0,
            digest: digest(&[]),
        };
        let mut segments = layer.map_or_else(Vec::new, |layer| layer.segments.clone());
        let tail = segments
            .last()
            .filter(|segment| segment.blocks < SEGMENT_BLOCKS)
            .is_some()
            .then(|| segments.pop().expect("tail segment"));
        let previous = layer.map_or(empty, |layer| layer.open.clone());
        Ok(Some(Self {
            dir: dir.to_owned(),
            generation,
            proj,
            rows,
            proj_len,
            rows_written: events,
            pending: Vec::new(),
            events,
            committed: layer.map_or(0, |layer| layer.committed),
            fence: layer.map_or_else(Vec::new, |layer| layer.fence.clone()),
            sniffed: layer.is_some_and(|layer| layer.sniffed),
            cap,
            blocks: layer.map_or_else(Vec::new, |layer| layer.blocks.clone()),
            segments,
            tail,
            postings: None,
            dirty: false,
            open: OpenBlock {
                first: previous.first,
                offset: previous.offset,
                bytes: Vec::new(),
                keys: Vec::new(),
                events: events - previous.first,
                loaded: false,
            },
            previous,
            uncommitted: None,
            retired: Vec::new(),
            full: false,
            broken: false,
            damaged: false,
            open_staging: budget.reserve_staging(0, cancel)?,
            postings_staging: budget.reserve_staging(0, cancel)?,
        }))
    }

    pub fn is_damaged(&self) -> bool {
        self.damaged
    }

    pub fn commit(
        &mut self,
        offset: u64,
        fence: &[u8],
        sniffed: bool,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        if self.full {
            return Ok(());
        }
        self.committed = offset;
        self.fence.clear();
        self.fence.extend_from_slice(fence);
        self.sniffed = sniffed;
        self.uncommitted = None;
        if self.open.bytes.len() >= BLOCK_BYTES || self.open.events >= BLOCK_EVENTS {
            self.close(budget, cancel)?;
        }
        Ok(())
    }

    fn load_open(
        &mut self,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<bool, SnapshotError> {
        if self.open.loaded {
            return Ok(true);
        }
        let len = (self.proj_len - self.open.offset) as usize;
        budget.charge_projection(len, 0, cancel)?;
        budget.extend_staging(&mut self.open_staging, len.saturating_mul(2), cancel)?;
        let Some(projected) = read_at(&self.proj, self.open.offset, len)
            .filter(|bytes| digest(bytes) == self.previous.digest)
            .and_then(|bytes| {
                let projected = Projected::decode_all(&bytes)?;
                self.open.bytes = bytes;
                Some(projected)
            })
            .filter(|projected| projected.len() == self.open.events)
        else {
            return Ok(false);
        };
        self.open.keys = projected.iter().flat_map(Projected::grams).collect();
        self.open.loaded = true;
        Ok(true)
    }

    fn load_postings(
        &mut self,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<bool, SnapshotError> {
        if self.postings.is_some() {
            return Ok(true);
        }
        let postings = match &self.tail {
            None => BTreeMap::new(),
            Some(tail) => match Segment::open(&self.dir, tail, budget, cancel)? {
                Ok(segment) => match segment.decode(budget, cancel)? {
                    Some(postings) => postings,
                    None => return Ok(false),
                },
                Err(_) => return Ok(false),
            },
        };
        budget.extend_staging(
            &mut self.postings_staging,
            postings.len().saturating_mul(48),
            cancel,
        )?;
        self.postings = Some(postings);
        Ok(true)
    }

    pub fn add(
        &mut self,
        line: LineSpan,
        sum: u64,
        entry: &Entry,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Option<Projected>, SnapshotError> {
        if self.full {
            return Ok(None);
        }
        let projected = Projected::of(entry);
        budget.charge_projection(projected.bytes(), 0, cancel)?;
        if !self.load_open(budget, cancel)? {
            self.damaged = true;
            self.full = true;
            return Ok(Some(projected));
        }
        let mut record = Vec::new();
        projected.encode(&mut record);
        if self.open.offset + (self.open.bytes.len() + record.len()) as u64 > self.cap {
            self.full = true;
            return Ok(Some(projected));
        }
        let keys: Vec<u32> = projected.grams().collect();
        budget.extend_staging(
            &mut self.open_staging,
            record.len().saturating_mul(2) + keys.len() * size_of::<u32>() + ROW_BYTES,
            cancel,
        )?;
        self.uncommitted = Some((
            self.open.bytes.len(),
            self.open.keys.len(),
            self.pending.len(),
        ));
        self.open.bytes.extend_from_slice(&record);
        self.open.keys.extend(keys);
        self.pending.extend_from_slice(&line.offset.to_le_bytes());
        self.pending
            .extend_from_slice(&(line.len as u32).to_le_bytes());
        self.pending.extend_from_slice(&sum.to_le_bytes());
        self.open.events += 1;
        self.events += 1;
        Ok(Some(projected))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let written = (self.proj_len - self.open.offset) as usize;
        self.proj
            .write_all_at(&self.open.bytes[written..], self.proj_len)?;
        self.proj_len = self.open.offset + self.open.bytes.len() as u64;
        self.rows
            .write_all_at(&self.pending, (self.rows_written * ROW_BYTES) as u64)?;
        self.rows_written = self.events;
        self.pending.clear();
        Ok(())
    }

    fn write_segment(
        &self,
        postings: &BTreeMap<u32, u64>,
        first: usize,
        blocks: usize,
    ) -> std::io::Result<SegmentRef> {
        let (bytes, head) = encode_segment(postings, blocks);
        let name = format!("seg-{}-{first}-{blocks}-{}", self.generation, nonce());
        let staged = self.dir.join(format!("{name}.tmp"));
        let written = open_private(&staged, true)
            .and_then(|file| file.write_all_at(&bytes, 0))
            .and_then(|()| std::fs::rename(&staged, self.dir.join(&name)));
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written?;
        Ok(SegmentRef {
            name,
            first,
            blocks,
            len: bytes.len() as u64,
            head,
        })
    }

    fn close(
        &mut self,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        if !self.load_postings(budget, cancel)? {
            self.damaged = true;
            self.full = true;
            return Ok(());
        }
        if self.flush().is_err() {
            self.broken = true;
            self.full = true;
            return Ok(());
        }
        let bit = (self.blocks.len() % SEGMENT_BLOCKS) as u32;
        self.open.keys.sort_unstable();
        self.open.keys.dedup();
        budget.extend_staging(
            &mut self.postings_staging,
            self.open.keys.len().saturating_mul(48),
            cancel,
        )?;
        let open_staging = budget.reserve_staging(0, cancel)?;
        let postings_staging = budget.reserve_staging(0, cancel)?;
        let postings = self.postings.as_mut().expect("postings loaded");
        for key in std::mem::take(&mut self.open.keys) {
            *postings.entry(key).or_default() |= 1 << bit;
        }
        self.blocks.push(BlockRef {
            first: self.open.first,
            offset: self.open.offset,
            digest: digest(&self.open.bytes),
        });
        self.open = OpenBlock {
            first: self.events,
            offset: self.proj_len,
            bytes: Vec::new(),
            keys: Vec::new(),
            events: 0,
            loaded: true,
        };
        self.open_staging = open_staging;
        self.dirty = true;
        if self.blocks.len() % SEGMENT_BLOCKS == 0 {
            self.postings_staging = postings_staging;
            match self.write_segment(
                self.postings.as_ref().expect("postings loaded"),
                self.blocks.len() - SEGMENT_BLOCKS,
                SEGMENT_BLOCKS,
            ) {
                Ok(segment) => self.segments.push(segment),
                Err(_) => {
                    self.broken = true;
                    self.full = true;
                    return Ok(());
                }
            }
            self.postings = Some(BTreeMap::new());
            self.retired.extend(self.tail.take().map(|tail| tail.name));
            self.dirty = false;
        }
        Ok(())
    }

    pub fn publish(&mut self) -> std::io::Result<IndexLayer> {
        if self.broken {
            return Err(std::io::Error::other("an index write failed this run"));
        }
        if let Some((bytes, keys, rows)) = self.uncommitted.take() {
            self.open.bytes.truncate(bytes);
            self.open.keys.truncate(keys);
            self.pending.truncate(rows);
            self.open.events -= 1;
            self.events -= 1;
        }
        let open = match self.open.loaded {
            true => {
                self.flush()?;
                BlockRef {
                    first: self.open.first,
                    offset: self.open.offset,
                    digest: digest(&self.open.bytes),
                }
            }
            false => self.previous.clone(),
        };
        let mut segments = self.segments.clone();
        match (self.dirty, &self.tail) {
            (true, _) => {
                let first = self.blocks.len() / SEGMENT_BLOCKS * SEGMENT_BLOCKS;
                let tail = self.write_segment(
                    self.postings.as_ref().expect("postings loaded"),
                    first,
                    self.blocks.len() - first,
                )?;
                self.retired
                    .extend(self.tail.replace(tail.clone()).map(|old| old.name));
                self.dirty = false;
                segments.push(tail);
            }
            (false, Some(tail)) => segments.push(tail.clone()),
            (false, None) => {}
        }
        Ok(IndexLayer {
            generation: self.generation.clone(),
            committed: self.committed,
            fence: self.fence.clone(),
            sniffed: self.sniffed,
            events: self.events,
            proj: self.proj_len,
            blocks: self.blocks.clone(),
            open,
            segments,
        })
    }

    pub fn retire(&mut self) {
        for name in self.retired.drain(..) {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
    }
}

use std::collections::HashSet;
use std::fs::{DirBuilder, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sonic_rs::Value;

use crate::scan_index::{open_private, IndexLayer, MIN_SOURCE_BYTES};
use crate::scan_stream::LineSpan;

pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const PREFIX_SEGMENT: u64 = 4 * 1024 * 1024;
pub const MAX_RECORDS: usize = 256;
pub const LOCK_STRIPES: usize = 1024;
pub const PROTOCOL_DIR: &str = "stripes-1";
const MAX_LISTED: usize = 4096;
const MAX_QUERIES: usize = 16;
const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct GrepCheckpoints {
    dir: PathBuf,
    producer: String,
    segment: u64,
    index_from: u64,
}

pub struct Prefix {
    span: u64,
    whole: Vec<(u64, u64)>,
    open: DefaultHasher,
    start: u64,
    end: u64,
    unread: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: usize,
    pub terminated: bool,
    pub digest: u64,
}

impl From<Span> for LineSpan {
    fn from(span: Span) -> Self {
        Self {
            offset: span.offset,
            len: span.len,
            terminated: span.terminated,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    pub index: usize,
    pub span: Span,
    pub pattern_ids: Option<Vec<usize>>,
    pub opens_window: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Queued {
    pub index: usize,
    pub span: Option<Span>,
    pub pattern_ids: Option<Vec<usize>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ReducerState {
    pub counts: Vec<usize>,
    pub matched_items: usize,
    pub coverage_complete: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileLayer {
    pub committed: u64,
    pub fence: Vec<u8>,
    pub sniffed: bool,
    pub events: usize,
    pub names: Vec<(String, String)>,
    pub prefix: Vec<(u64, u64)>,
    pub index: Option<IndexLayer>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueryLayer {
    pub key: String,
    pub indexed: bool,
    pub committed: u64,
    pub parsed: usize,
    pub decided: usize,
    pub emitted: usize,
    pub last_emitted: Option<usize>,
    pub last_hit: Option<usize>,
    pub stopped: Option<usize>,
    pub reducer: ReducerState,
    pub referenced: Vec<(String, String)>,
    pub replay: Vec<Replayed>,
    pub queue: Vec<Queued>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceRecord {
    pub key: String,
    pub size: u64,
    pub revision: String,
    pub file: FileLayer,
    pub queries: Vec<QueryLayer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version {
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

pub struct Loaded {
    pub record: SourceRecord,
    version: Version,
}

pub struct Saved {
    pub published: bool,
    pub read: usize,
}

pub fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn lock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn try_lock(path: &Path) -> Option<File> {
    open_private(path, true).ok().filter(lock)
}

fn stripe(key: &str) -> usize {
    let digest = Sha256::digest(key.as_bytes());
    usize::from(u16::from_be_bytes([digest[0], digest[1]])) % LOCK_STRIPES
}

fn version(metadata: &std::fs::Metadata) -> Version {
    Version {
        inode: metadata.ino(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

fn private(path: &Path) -> bool {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .and_then(|()| std::fs::symlink_metadata(path))
        .is_ok_and(|metadata| {
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
        })
}

fn dir_bytes(path: &Path) -> u64 {
    std::fs::read_dir(path).map_or(0, |entries| {
        entries
            .take(MAX_LISTED)
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum()
    })
}

impl FileLayer {
    fn proven(&self) -> bool {
        self.prefix.last().map_or(0, |(end, _)| *end) == self.committed
    }

    fn agreed(&self, other: &Self) -> u64 {
        self.prefix
            .iter()
            .zip(&other.prefix)
            .take_while(|(ours, theirs)| ours == theirs)
            .last()
            .map_or(0, |((end, _), _)| *end)
    }
}

impl SourceRecord {
    pub fn merge(latest: Option<Self>, trusted: bool, ours: Self, builds: bool) -> Option<Self> {
        let Some(latest) = latest else {
            return ours.file.proven().then_some(ours);
        };
        let shared = (!trusted).then(|| ours.file.agreed(&latest.file));
        let crosses = |layer: &QueryLayer| {
            shared.is_none_or(|shared| !layer.indexed && layer.committed <= shared)
        };
        let query = ours
            .queries
            .first()
            .cloned()
            .expect("a saved record carries its query layer");
        let ours_wins = ours.file.committed > latest.file.committed && ours.file.proven();
        let (mut record, other) = if ours_wins {
            (ours, latest)
        } else {
            (latest, ours)
        };
        if builds != ours_wins {
            if let Some(index) = other
                .file
                .index
                .filter(|index| shared.is_none_or(|shared| index.committed <= shared))
            {
                record.file.index = Some(index);
            }
        }
        let carried = ours_wins || crosses(&query);
        let mut queries: Vec<QueryLayer> = std::mem::take(&mut record.queries)
            .into_iter()
            .chain(other.queries.into_iter().filter(|layer| crosses(layer)))
            .filter(|layer| !carried || layer.key != query.key)
            .collect();
        if carried {
            queries.insert(0, query);
        }
        queries.truncate(MAX_QUERIES);
        record.queries = queries;
        Some(record)
    }
}

impl Prefix {
    pub fn new(span: u64) -> Self {
        Self {
            span,
            whole: Vec::new(),
            open: DefaultHasher::new(),
            start: 0,
            end: 0,
            unread: None,
        }
    }

    pub fn adopt(span: u64, segments: &[(u64, u64)]) -> Self {
        let open = segments
            .split_last()
            .map(|(&(end, sum), rest)| (end, sum, rest.last().map_or(0, |(end, _)| *end)))
            .filter(|(end, _, start)| end - start < span);
        let whole = segments[..segments.len() - usize::from(open.is_some())].to_vec();
        let start = whole.last().map_or(0, |(end, _)| *end);
        Self {
            span,
            whole,
            open: DefaultHasher::new(),
            start,
            end: open.map_or(start, |(end, _, _)| end),
            unread: open.map(|(_, sum, _)| sum),
        }
    }

    pub fn span(&self) -> u64 {
        self.span
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn room(&self) -> u64 {
        self.start + self.span - self.end
    }

    pub fn unread(&self) -> Option<LineSpan> {
        self.unread.map(|_| LineSpan {
            offset: self.start,
            len: (self.end - self.start) as usize,
            terminated: false,
        })
    }

    pub fn seed(&mut self, bytes: &[u8]) -> bool {
        self.open.write(bytes);
        self.unread.take() == Some(self.open.finish())
    }

    pub fn write(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (head, rest) = bytes.split_at((self.room() as usize).min(bytes.len()));
            self.open.write(head);
            self.end += head.len() as u64;
            if self.room() == 0 {
                self.whole.push((self.end, self.open.finish()));
                self.open = DefaultHasher::new();
                self.start = self.end;
            }
            bytes = rest;
        }
    }

    pub fn agrees(&self, segments: &[(u64, u64)]) -> bool {
        self.whole
            .iter()
            .zip(segments)
            .all(|(ours, theirs)| ours == theirs)
    }

    pub fn segments(&self) -> Vec<(u64, u64)> {
        self.whole
            .iter()
            .copied()
            .chain(
                (self.end > self.start)
                    .then(|| (self.end, self.unread.unwrap_or_else(|| self.open.finish()))),
            )
            .collect()
    }
}

impl GrepCheckpoints {
    pub fn new(dir: PathBuf, producer: String) -> Self {
        Self {
            dir: dir.join(PROTOCOL_DIR),
            producer,
            segment: PREFIX_SEGMENT,
            index_from: MIN_SOURCE_BYTES,
        }
    }

    pub fn segmented(self, segment: u64) -> Self {
        Self { segment, ..self }
    }

    pub fn segment(&self) -> u64 {
        self.segment
    }

    pub fn indexing_from(self, index_from: u64) -> Self {
        Self { index_from, ..self }
    }

    pub fn indexes(&self, size: u64) -> bool {
        size >= self.index_from
    }

    pub fn key(&self, binding: Value) -> Result<String, String> {
        let binding = sonic_rs::json!({
            "version": "grep-checkpoint/5",
            "parser": crate::snapshot::PARSER_VERSION,
            "producer": self.producer,
            "binding": binding,
        });
        Ok(format!(
            "{:x}",
            Sha256::digest(crate::ids::canonical_json(&binding)?.as_bytes())
        ))
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn index_dir(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.idx"))
    }

    fn lock_dir(&self) -> PathBuf {
        self.dir.join("locks")
    }

    fn build_lock_path(&self, key: &str) -> PathBuf {
        self.lock_dir()
            .join(format!("build-{:03x}.lock", stripe(key)))
    }

    fn record_lock_path(&self, key: &str) -> PathBuf {
        self.lock_dir()
            .join(format!("record-{:03x}.lock", stripe(key)))
    }

    fn private_dir(&self) -> bool {
        private(&self.dir)
    }

    fn locks(&self) -> bool {
        self.private_dir() && private(&self.lock_dir())
    }

    pub fn build_lock(&self, key: &str) -> Option<File> {
        self.locks()
            .then(|| try_lock(&self.build_lock_path(key)))
            .flatten()
            .filter(|_| private(&self.index_dir(key)))
    }

    pub fn load(&self, key: &str, limit: usize) -> (Option<Loaded>, usize) {
        if !self.private_dir() {
            return (None, 0);
        }
        let (loaded, bytes, corrupt) = self.read(key, limit);
        if corrupt {
            self.discard(key);
        }
        (loaded, bytes)
    }

    fn read(&self, key: &str, limit: usize) -> (Option<Loaded>, usize, bool) {
        let Ok(mut file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(key))
        else {
            return (None, 0, false);
        };
        let Ok(metadata) = file.metadata() else {
            return (None, 0, false);
        };
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() > MAX_RECORD_BYTES.min(limit) as u64
        {
            return (None, 0, false);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if (&mut file)
            .take(metadata.len())
            .read_to_end(&mut bytes)
            .is_err()
        {
            return (None, bytes.len(), false);
        }
        match sonic_rs::from_slice::<SourceRecord>(&bytes) {
            Ok(record) if record.key == key => (
                Some(Loaded {
                    record,
                    version: version(&metadata),
                }),
                bytes.len(),
                false,
            ),
            _ => (None, bytes.len(), true),
        }
    }

    pub fn discard(&self, key: &str) {
        let _ = std::fs::remove_file(self.path(key));
        self.discard_index(key);
    }

    pub fn discard_index(&self, key: &str) {
        let _ = std::fs::remove_dir_all(self.index_dir(key));
    }

    pub fn save(
        &self,
        ours: SourceRecord,
        builds: bool,
        basis: Option<&Loaded>,
        limit: usize,
    ) -> std::io::Result<Saved> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        if !self.locks() {
            return Ok(Saved {
                published: false,
                read: 0,
            });
        }
        let Some(guard) = try_lock(&self.record_lock_path(&ours.key)) else {
            return Ok(Saved {
                published: false,
                read: 0,
            });
        };
        let current = std::fs::symlink_metadata(self.path(&ours.key))
            .ok()
            .map(|metadata| version(&metadata));
        let (latest, trusted, read) = match basis {
            Some(basis) if current == Some(basis.version) => (Some(basis.record.clone()), true, 0),
            _ => match self.read(&ours.key, limit) {
                (None, read, false) if current.is_some() => {
                    return Ok(Saved {
                        published: false,
                        read,
                    })
                }
                (latest, read, _) => (latest.map(|loaded| loaded.record), false, read),
            },
        };
        let fresh = ours.file.index.clone();
        let Some(record) = SourceRecord::merge(latest, trusted, ours, builds) else {
            return Ok(Saved {
                published: false,
                read,
            });
        };
        let bytes = sonic_rs::to_vec(&record).map_err(std::io::Error::other)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Ok(Saved {
                published: false,
                read,
            });
        }
        let staged = self.dir.join(format!(
            "{}.{}.{}.tmp",
            record.key,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staged)
            .and_then(|mut file| file.write_all(&bytes))
            .and_then(|()| std::fs::rename(&staged, self.path(&record.key)));
        if written.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        written?;
        drop(guard);
        self.evict(builds.then_some(record.key.as_str()));
        Ok(Saved {
            published: record.file.index == fresh,
            read,
        })
    }

    fn victim(&self, key: &str, holder: Option<&str>) -> Option<(Option<File>, File)> {
        let build = if holder.is_some_and(|holder| stripe(holder) == stripe(key)) {
            None
        } else {
            Some(try_lock(&self.build_lock_path(key))?)
        };
        Some((build, try_lock(&self.record_lock_path(key))?))
    }

    fn evict(&self, holder: Option<&str>) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut records = Vec::new();
        let mut dirs = Vec::new();
        for entry in entries.take(MAX_LISTED).flatten() {
            let path = entry.path();
            let (Some(extension), Some(key)) = (
                path.extension().and_then(|extension| extension.to_str()),
                path.file_stem().and_then(|stem| stem.to_str()),
            ) else {
                continue;
            };
            match extension {
                "json" => {
                    if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified())
                    {
                        records.push((modified, key.to_owned()));
                    }
                }
                "idx" => dirs.push(key.to_owned()),
                _ => {}
            }
        }
        records.sort();
        let excess = records.len().saturating_sub(MAX_RECORDS);
        for (_, key) in records.drain(..excess) {
            if let Some(_held) = self.victim(&key, holder) {
                self.discard(&key);
            }
        }
        if holder.is_none() {
            return;
        }
        let kept: HashSet<&str> = records.iter().map(|(_, key)| key.as_str()).collect();
        for key in dirs.iter().filter(|key| !kept.contains(key.as_str())) {
            if let Some(_held) = self.victim(key, holder) {
                self.discard_index(key);
            }
        }
        let mut total = 0u64;
        for (_, key) in records.iter().rev() {
            match total.saturating_add(dir_bytes(&self.index_dir(key))) {
                within if within <= MAX_INDEX_BYTES => total = within,
                _ => {
                    if let Some(_held) = self.victim(key, holder) {
                        self.discard_index(key);
                    }
                }
            }
        }
    }
}

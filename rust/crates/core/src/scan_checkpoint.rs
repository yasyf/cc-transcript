use std::fs::{DirBuilder, OpenOptions};
use std::hash::{DefaultHasher, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sonic_rs::Value;

use crate::scan_stream::LineSpan;

pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const PREFIX_SEGMENT: u64 = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 256;
const MAX_LISTED: usize = 1024;
const MAX_QUERIES: usize = 16;

pub struct GrepCheckpoints {
    dir: PathBuf,
    producer: String,
    segment: u64,
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
    pub span: Span,
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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QueryLayer {
    pub key: String,
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

impl SourceRecord {
    pub fn merge(
        existing: Option<Self>,
        key: String,
        size: u64,
        revision: String,
        file: FileLayer,
        query: QueryLayer,
    ) -> Self {
        let mut record = match existing {
            Some(record) if record.file.committed >= file.committed => Self {
                size,
                revision,
                ..record
            },
            Some(record) => Self {
                key,
                size,
                revision,
                file,
                queries: record.queries,
            },
            None => Self {
                key,
                size,
                revision,
                file,
                queries: Vec::new(),
            },
        };
        record.queries.retain(|layer| layer.key != query.key);
        record.queries.insert(0, query);
        record.queries.truncate(MAX_QUERIES);
        record
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
            dir,
            producer,
            segment: PREFIX_SEGMENT,
        }
    }

    pub fn segmented(self, segment: u64) -> Self {
        Self { segment, ..self }
    }

    pub fn segment(&self) -> u64 {
        self.segment
    }

    pub fn key(&self, binding: Value) -> Result<String, String> {
        let binding = sonic_rs::json!({
            "version": "grep-checkpoint/4",
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

    fn private_dir(&self) -> bool {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .and_then(|()| std::fs::symlink_metadata(&self.dir))
            .is_ok_and(|metadata| {
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0
            })
    }

    pub fn load(&self, key: &str, limit: usize) -> (Option<SourceRecord>, usize) {
        if !self.private_dir() {
            return (None, 0);
        }
        let Ok(mut file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(key))
        else {
            return (None, 0);
        };
        let Ok(metadata) = file.metadata() else {
            return (None, 0);
        };
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() > MAX_RECORD_BYTES.min(limit) as u64
        {
            return (None, 0);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if (&mut file)
            .take(metadata.len())
            .read_to_end(&mut bytes)
            .is_err()
        {
            return (None, bytes.len());
        }
        match sonic_rs::from_slice::<SourceRecord>(&bytes) {
            Ok(record) if record.key == key => (Some(record), bytes.len()),
            _ => {
                self.discard(key);
                (None, bytes.len())
            }
        }
    }

    pub fn discard(&self, key: &str) {
        let _ = std::fs::remove_file(self.path(key));
    }

    pub fn save(&self, record: &SourceRecord) -> std::io::Result<()> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let bytes = sonic_rs::to_vec(record).map_err(std::io::Error::other)?;
        if bytes.len() > MAX_RECORD_BYTES || !self.private_dir() {
            return Ok(());
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
        self.evict();
        Ok(())
    }

    fn evict(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut records: Vec<(std::time::SystemTime, PathBuf)> = entries
            .take(MAX_LISTED)
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                (path.extension()? == "json").then_some(())?;
                Some((entry.metadata().ok()?.modified().ok()?, path))
            })
            .collect();
        if records.len() <= MAX_RECORDS {
            return;
        }
        records.sort();
        for (_, path) in &records[..records.len() - MAX_RECORDS] {
            let _ = std::fs::remove_file(path);
        }
    }
}

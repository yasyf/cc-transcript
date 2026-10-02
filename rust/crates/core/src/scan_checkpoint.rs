use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sonic_rs::Value;

use crate::scan_stream::LineSpan;

const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 256;
const MAX_LISTED: usize = 1024;

pub struct GrepCheckpoints {
    dir: PathBuf,
    producer: String,
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
pub struct Checkpoint {
    pub key: String,
    pub size: u64,
    pub revision: String,
    pub committed: u64,
    pub fence: Vec<u8>,
    pub sniffed: bool,
    pub parsed: usize,
    pub decided: usize,
    pub emitted: usize,
    pub last_emitted: Option<usize>,
    pub last_hit: Option<usize>,
    pub stopped: Option<usize>,
    pub reducer: ReducerState,
    pub names: Vec<(String, String, bool)>,
    pub replay: Vec<Replayed>,
    pub queue: Vec<Queued>,
}

impl GrepCheckpoints {
    pub fn new(dir: PathBuf, producer: String) -> Self {
        Self { dir, producer }
    }

    pub fn key(&self, binding: Value) -> Result<String, String> {
        let binding = sonic_rs::json!({
            "version": "grep-checkpoint/1",
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

    pub fn load(&self, key: &str, limit: usize) -> (Option<Checkpoint>, usize) {
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
        match sonic_rs::from_slice::<Checkpoint>(&bytes) {
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

    pub fn save(&self, record: &Checkpoint) -> std::io::Result<()> {
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

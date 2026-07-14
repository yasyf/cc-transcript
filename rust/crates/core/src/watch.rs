//! Live-tail cursor: the byte-offset state machine that yields each appended
//! transcript event exactly once. Ported from `cc_transcript/watch.py`; the async
//! poll-forever wrapper stays in the Python facade — this is the pure [`tick`]
//! step over a [`TailState`], directly drivable by tests and embedders.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::Datelike;
use sonic_rs::{JsonValueTrait, Value};

use crate::discovery::{is_subagent_path, mtime_secs};
use crate::parse::parse_entry;
use crate::types::Entry;

/// How many yielded event uuids each file's dedupe set retains.
pub const SEEN_LIMIT: usize = 4096;

/// One transcript event freshly appended to a watched file: the file it came
/// from, the session it belongs to, whether the file is a subagent sidechain,
/// and the parsed entry.
pub struct WatchEvent {
    pub path: PathBuf,
    pub session_id: String,
    pub is_sidechain: bool,
    pub event: Entry,
}

/// One watched file's tail progress. `offset` is always the end of the last
/// complete line (a partial trailing line stays unconsumed); `size`/`mtime` are
/// the last processed stat (both `-1` sentinels until the first read); `seen`
/// is the insertion-ordered, `SEEN_LIMIT`-bounded set of yielded uuids.
pub struct TailCursor {
    pub offset: i64,
    pub size: i64,
    pub mtime: f64,
    pub session_id: Option<String>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl TailCursor {
    fn new(offset: i64, size: i64, mtime: f64) -> Self {
        Self {
            offset,
            size,
            mtime,
            session_id: None,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
        }
    }

    /// The yielded uuids in insertion order — the Python `list(cursor.seen)` view.
    pub fn seen(&self) -> impl Iterator<Item = &str> {
        self.seen_order.iter().map(String::as_str)
    }

    fn clear_seen(&mut self) {
        self.seen.clear();
        self.seen_order.clear();
    }

    fn remember(&mut self, uuid: String) {
        self.seen_order.push_back(uuid.clone());
        self.seen.insert(uuid);
        while self.seen.len() > SEEN_LIMIT {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

/// The tailer's whole mutable state: one cursor per discovered file, plus
/// whether the priming discovery pass has run.
#[derive(Default)]
pub struct TailState {
    pub cursors: HashMap<PathBuf, TailCursor>,
    pub primed: bool,
}

/// Run one poll step: discover changes under `roots` and drain them.
///
/// Mirrors `cc_transcript.watch.tick`: a file first seen on the priming pass
/// starts at EOF unless `from_start`; a file appearing later starts at byte 0; a
/// file that shrank below the cursor was rewritten (compaction), so the cursor
/// resets and its dedupe set clears. The cursor only ever advances past the last
/// complete line, and lines the decoder rejects are skipped.
///
/// Returns the newly appended events, files in path order, lines in file order.
pub fn tick(state: &mut TailState, roots: &[PathBuf], from_start: bool) -> Vec<WatchEvent> {
    let stats = scan(roots);
    let priming = !state.primed;
    state.primed = true;
    let mut paths: Vec<&PathBuf> = stats.keys().collect();
    paths.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    let mut events = Vec::new();
    for path in paths {
        let (size, mtime) = stats[path];
        let cursor =
            state
                .cursors
                .entry(path.clone())
                .or_insert_with(|| match priming && !from_start {
                    true => TailCursor::new(size, size, mtime),
                    false => TailCursor::new(0, -1, -1.0),
                });
        if size < cursor.offset {
            cursor.offset = 0;
            cursor.clear_seen();
        } else if size == cursor.size && mtime == cursor.mtime {
            continue;
        }
        let Ok(chunk) = read_from(path, cursor.offset) else {
            continue;
        };
        cursor.size = size;
        cursor.mtime = mtime;
        let (complete, partial): (&[u8], &[u8]) = match chunk.iter().rposition(|&b| b == b'\n') {
            Some(i) => (&chunk[..i], &chunk[i + 1..]),
            None => (&[], &chunk[..]),
        };
        cursor.offset += (chunk.len() - partial.len()) as i64;
        for line in complete.split(|&b| b == b'\n') {
            let Some(event) = decode(line) else {
                continue;
            };
            if let Some(meta) = event.meta() {
                if cursor.seen.contains(meta.uuid.as_str()) {
                    continue;
                }
                cursor.remember(meta.uuid.clone());
            }
            events.push(WatchEvent {
                path: path.clone(),
                session_id: session_of(cursor, path, &event),
                is_sidechain: is_subagent_path(path),
                event,
            });
        }
    }
    events
}

fn scan(roots: &[PathBuf]) -> HashMap<PathBuf, (i64, f64)> {
    let mut found: HashMap<PathBuf, (i64, f64)> = HashMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        walk_jsonl(root, &mut |path| {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("._"))
            {
                return;
            }
            if let Ok(md) = fs::metadata(path) {
                found.insert(path.to_path_buf(), (md.len() as i64, mtime_secs(&md)));
            }
        });
    }
    found
}

fn walk_jsonl(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            visit(&path);
        }
    }
}

fn read_from(path: &Path, offset: i64) -> std::io::Result<Vec<u8>> {
    let mut handle = File::open(path)?;
    handle.seek(SeekFrom::Start(offset as u64))?;
    let mut buffer = Vec::new();
    handle.read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Decode one complete line, treating any malformed payload as garbage: a
/// blank line, non-JSON, a non-object, a typed-parse failure, or a timestamp
/// outside Python's `datetime` year range (1–9999) all yield `None`, mirroring
/// `watch.decode`'s catch of the eager `datetime.fromisoformat` in `parse_meta`.
fn decode(line: &[u8]) -> Option<Entry> {
    if line.iter().all(is_bytes_space) {
        return None;
    }
    let value: Value = sonic_rs::from_slice(line).ok()?;
    if !value.is_object() {
        return None;
    }
    let entry = parse_entry(value).ok()?;
    match entry.meta() {
        Some(meta) if !(1..=9999).contains(&meta.timestamp.year()) => None,
        _ => Some(entry),
    }
}

// Python `bytes.strip()` whitespace: HT, LF, VT, FF, CR, SP. `is_ascii_whitespace`
// omits VT (U+000B), so it is spelled out here.
fn is_bytes_space(byte: &u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

fn session_of(cursor: &mut TailCursor, path: &Path, event: &Entry) -> String {
    let session = event
        .session_id()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cursor.session_id.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| path_session_id(path));
    cursor.session_id = Some(session.clone());
    session
}

fn path_session_id(path: &Path) -> String {
    match is_subagent_path(path) {
        true => path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        false => path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, bytes: &[u8]) {
        let mut handle = File::create(path).unwrap();
        handle.write_all(bytes).unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut handle = fs::OpenOptions::new().append(true).open(path).unwrap();
        handle.write_all(bytes).unwrap();
    }

    fn user_line(uuid: &str, session: &str, sec: u32) -> String {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"sessionId\":\"{session}\",\
             \"timestamp\":\"2026-01-01T00:00:{sec:02}.000Z\",\
             \"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}"
        )
    }

    fn uuids(events: &[WatchEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| e.event.meta().map(|m| m.uuid.clone()).unwrap_or_default())
            .collect()
    }

    #[test]
    fn priming_skips_history_then_tails_appends() {
        let dir = std::env::temp_dir().join(format!("watch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write(
            &path,
            format!("{}\n{}\n", user_line("a", "s", 0), user_line("b", "s", 1)).as_bytes(),
        );

        let mut state = TailState::default();
        let roots = [dir.clone()];
        assert!(
            tick(&mut state, &roots, false).is_empty(),
            "priming skips history"
        );

        append(&path, format!("{}\n", user_line("c", "s", 2)).as_bytes());
        let events = tick(&mut state, &roots, false);
        assert_eq!(uuids(&events), ["c"]);
        assert_eq!(events[0].session_id, "s");
        assert!(!events[0].is_sidechain);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_line_waits_for_its_newline() {
        let dir = std::env::temp_dir().join(format!("watch-partial-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write(&path, b"");
        let mut state = TailState::default();
        let roots = [dir.clone()];
        tick(&mut state, &roots, true);

        append(&path, user_line("a", "s", 0).as_bytes()); // no newline yet
        assert!(
            tick(&mut state, &roots, true).is_empty(),
            "partial line held"
        );
        append(&path, b"\n");
        assert_eq!(uuids(&tick(&mut state, &roots, true)), ["a"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncation_resets_and_rereads() {
        let dir = std::env::temp_dir().join(format!("watch-trunc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write(
            &path,
            format!("{}\n{}\n", user_line("a", "s", 0), user_line("b", "s", 1)).as_bytes(),
        );
        let mut state = TailState::default();
        let roots = [dir.clone()];
        assert_eq!(uuids(&tick(&mut state, &roots, true)), ["a", "b"]);

        write(&path, format!("{}\n", user_line("c", "s", 2)).as_bytes()); // shorter: compaction
        assert_eq!(uuids(&tick(&mut state, &roots, true)), ["c"]);
        assert_eq!(state.cursors[&path].seen().collect::<Vec<_>>(), ["c"]);

        fs::remove_dir_all(&dir).ok();
    }
}

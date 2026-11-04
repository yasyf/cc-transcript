//! A persistent listing of every transcript under a projects root, refreshed on
//! read from the FSEvents history instead of a stat of every file.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use cc_transcript_core::discovery::mtime_secs;

const WALK_THREADS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stamp {
    mtime: f64,
    size: u64,
    inode: u64,
    link: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Epoch {
    volume: String,
    root: u64,
    event: u64,
}

#[derive(Debug, PartialEq)]
struct Index {
    root: PathBuf,
    epoch: Option<Epoch>,
    records: BTreeMap<PathBuf, Stamp>,
}

pub struct Transcript {
    pub path: PathBuf,
    pub mtime: f64,
    pub size: u64,
}

pub fn transcripts(root: &Path) -> Vec<Transcript> {
    let Ok(canonical) = fs::canonicalize(root) else {
        return Vec::new();
    };
    refreshed(&canonical)
        .records
        .into_iter()
        .map(|(relative, stamp)| Transcript {
            path: root.join(relative),
            mtime: stamp.mtime,
            size: stamp.size,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn refreshed(root: &Path) -> Index {
    persisted::refreshed(
        root,
        &crate::target::home_dir()
            .join(".claude")
            .join("cc-transcript")
            .join("index"),
    )
}

#[cfg(not(target_os = "macos"))]
fn refreshed(root: &Path) -> Index {
    full(root)
}

fn full(root: &Path) -> Index {
    Index {
        root: root.to_owned(),
        epoch: None,
        records: walk(root, Path::new("")),
    }
}

fn stamp(path: &Path) -> Option<Stamp> {
    path.file_name()?
        .as_bytes()
        .ends_with(b".jsonl")
        .then_some(())?;
    let entry = fs::symlink_metadata(path).ok()?;
    let link = entry.is_symlink();
    let metadata = if link {
        fs::metadata(path).ok()?
    } else {
        entry
    };
    metadata.is_file().then(|| Stamp {
        mtime: mtime_secs(&metadata),
        size: metadata.len(),
        inode: metadata.ino(),
        link,
    })
}

fn walk(root: &Path, start: &Path) -> BTreeMap<PathBuf, Stamp> {
    let state = Mutex::new((vec![start.to_owned()], 0usize));
    let ready = Condvar::new();
    let found = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..WALK_THREADS {
            scope.spawn(|| {
                let mut guard = state.lock().expect("walk queue");
                loop {
                    if let Some(relative) = guard.0.pop() {
                        guard.1 += 1;
                        drop(guard);
                        let mut children = Vec::new();
                        let mut stamps = Vec::new();
                        for entry in fs::read_dir(root.join(&relative))
                            .into_iter()
                            .flatten()
                            .flatten()
                        {
                            let child = relative.join(entry.file_name());
                            match entry.file_type() {
                                Ok(kind) if kind.is_dir() => children.push(child),
                                _ => {
                                    stamps.extend(stamp(&entry.path()).map(|stamp| (child, stamp)))
                                }
                            }
                        }
                        found.lock().expect("walk results").extend(stamps);
                        guard = state.lock().expect("walk queue");
                        guard.0.append(&mut children);
                        guard.1 -= 1;
                        ready.notify_all();
                    } else if guard.1 == 0 {
                        ready.notify_all();
                        return;
                    } else {
                        guard = ready.wait(guard).expect("walk queue");
                    }
                }
            });
        }
    });
    found.into_inner().expect("walk results")
}

#[cfg(target_os = "macos")]
mod persisted {
    use std::collections::{BTreeMap, HashMap};
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use sha2::{Digest, Sha256};

    use super::{fsevents, full, stamp, walk, Epoch, Index, Stamp};

    const MAGIC: &[u8] = b"cc-transcript-index 2\n";
    const KEPT_INDEXES: usize = 8;

    pub(super) fn refreshed(root: &Path, store: &Path) -> Index {
        let file = index_file(store, root);
        let volume = fs::metadata(root)
            .ok()
            .and_then(|metadata| Some((fsevents::device_uuid(metadata.dev())?, metadata.ino())));
        let upto = fsevents::current_event_id();
        let stored = fs::read(&file)
            .ok()
            .and_then(|bytes| decode(&bytes))
            .filter(|index| index.root == root);
        let mut index = match (stored, &volume) {
            (Some(mut index), Some((uuid, inode))) => match index.epoch.take() {
                Some(epoch) if epoch.volume == *uuid && epoch.root == *inode => {
                    let replayed = fsevents::changes(root, epoch.event)
                        .is_some_and(|changes| apply(&mut index, root, changes));
                    if replayed {
                        index
                    } else {
                        full(root)
                    }
                }
                _ => full(root),
            },
            _ => full(root),
        };
        index.epoch = volume.map(|(volume, root)| Epoch {
            volume,
            root,
            event: upto,
        });
        save(store, &file, &index);
        index
    }

    pub(super) fn index_file(store: &Path, root: &Path) -> PathBuf {
        let digest = Sha256::digest(root.as_os_str().as_bytes());
        store.join(format!(
            "{}.ctix",
            digest[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    fn apply(index: &mut Index, root: &Path, changes: HashMap<PathBuf, u32>) -> bool {
        for (path, flags) in changes {
            let rescan = flags & fsevents::MUST_SCAN_SUB_DIRS != 0;
            if flags & fsevents::DROPPED != 0 {
                return false;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                if rescan && root.starts_with(&path) {
                    return false;
                }
                continue;
            };
            let structural =
                rescan || flags & fsevents::ITEM_IS_DIR != 0 && flags & fsevents::RESTRUCTURED != 0;
            if relative.as_os_str().is_empty() {
                if structural {
                    return false;
                }
                continue;
            }
            if structural {
                index
                    .records
                    .retain(|known, _| !known.starts_with(relative));
            } else {
                index.records.remove(relative);
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {
                    if structural {
                        index.records.extend(walk(root, relative));
                    }
                }
                _ => index
                    .records
                    .extend(stamp(&path).map(|stamp| (relative.to_owned(), stamp))),
            }
        }
        let links: Vec<PathBuf> = index
            .records
            .iter()
            .filter(|(_, stamp)| stamp.link)
            .map(|(relative, _)| relative.clone())
            .collect();
        for relative in links {
            match stamp(&root.join(&relative)) {
                Some(stamp) => index.records.insert(relative, stamp),
                None => index.records.remove(&relative),
            };
        }
        true
    }

    fn save(store: &Path, file: &Path, index: &Index) {
        if fs::create_dir_all(store).is_err() {
            return;
        }
        let staged = file.with_extension(format!("{}.tmp", std::process::id()));
        let written = fs::File::create(&staged)
            .and_then(|mut out| out.write_all(&encode(index)))
            .and_then(|()| fs::rename(&staged, file));
        if written.is_err() {
            let _ = fs::remove_file(&staged);
        }
        let mut kept: Vec<_> = fs::read_dir(store)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .collect();
        kept.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in kept.into_iter().skip(KEPT_INDEXES) {
            let _ = fs::remove_file(path);
        }
    }

    pub(super) fn encode(index: &Index) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        let field = |out: &mut Vec<u8>, bytes: &[u8]| {
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        };
        field(&mut out, index.root.as_os_str().as_bytes());
        let epoch = index.epoch.as_ref();
        field(
            &mut out,
            epoch.map_or(&b""[..], |epoch| epoch.volume.as_bytes()),
        );
        for word in [
            epoch.map_or(0, |epoch| epoch.root),
            epoch.map_or(0, |epoch| epoch.event),
            index.records.len() as u64,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        for (path, stamp) in &index.records {
            for word in [
                stamp.mtime.to_bits(),
                stamp.size,
                stamp.inode,
                u64::from(stamp.link),
            ] {
                out.extend_from_slice(&word.to_le_bytes());
            }
            field(&mut out, path.as_os_str().as_bytes());
        }
        out
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Index> {
        let mut fields = Fields(bytes.strip_prefix(MAGIC)?);
        let root = PathBuf::from(OsStr::from_bytes(fields.field()?));
        let volume = String::from_utf8(fields.field()?.to_vec()).ok()?;
        let epoch = Epoch {
            volume,
            root: fields.word()?,
            event: fields.word()?,
        };
        let mut records = BTreeMap::new();
        for _ in 0..fields.word()? {
            let stamp = Stamp {
                mtime: f64::from_bits(fields.word()?),
                size: fields.word()?,
                inode: fields.word()?,
                link: fields.word()? != 0,
            };
            records.insert(PathBuf::from(OsStr::from_bytes(fields.field()?)), stamp);
        }
        fields.0.is_empty().then_some(Index {
            root,
            epoch: (!epoch.volume.is_empty()).then_some(epoch),
            records,
        })
    }

    struct Fields<'a>(&'a [u8]);

    impl<'a> Fields<'a> {
        fn take(&mut self, len: usize) -> Option<&'a [u8]> {
            let (head, tail) = self.0.split_at_checked(len)?;
            self.0 = tail;
            Some(head)
        }

        fn word(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }

        fn field(&mut self) -> Option<&'a [u8]> {
            let len = u32::from_le_bytes(self.take(4)?.try_into().ok()?);
            self.take(len as usize)
        }
    }
}

#[cfg(target_os = "macos")]
mod fsevents {
    use std::collections::HashMap;
    use std::ffi::{c_char, c_void, CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    pub const MUST_SCAN_SUB_DIRS: u32 = 0x1;
    pub const DROPPED: u32 = 0x2 | 0x4 | 0x8 | 0x20;
    pub const RESTRUCTURED: u32 = 0x100 | 0x200 | 0x800;
    pub const ITEM_IS_DIR: u32 = 0x20000;
    const HISTORY_DONE: u32 = 0x10;
    const NO_DEFER: u32 = 0x2;
    const FILE_EVENTS: u32 = 0x10;
    const UTF8: u32 = 0x0800_0100;
    const REPLAY_WAIT: Duration = Duration::from_secs(30);

    type Ref = *const c_void;
    type Stream = *mut c_void;
    type Callback = extern "C" fn(Stream, *mut c_void, usize, *mut c_void, *const u32, *const u64);

    #[repr(C)]
    struct Context {
        version: isize,
        info: *mut c_void,
        retain: Ref,
        release: Ref,
        describe: Ref,
    }

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn FSEventsGetCurrentEventId() -> u64;
        fn FSEventsCopyUUIDForDevice(device: i32) -> Ref;
        fn FSEventStreamCreate(
            allocator: Ref,
            callback: Callback,
            context: *const Context,
            paths: Ref,
            since: u64,
            latency: f64,
            flags: u32,
        ) -> Stream;
        fn FSEventStreamSetDispatchQueue(stream: Stream, queue: *mut c_void);
        fn FSEventStreamStart(stream: Stream) -> u8;
        fn FSEventStreamStop(stream: Stream);
        fn FSEventStreamInvalidate(stream: Stream);
        fn FSEventStreamRelease(stream: Stream);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
        fn CFStringGetCString(text: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFArrayCreate(allocator: Ref, values: *const Ref, count: isize, callbacks: Ref) -> Ref;
        fn CFUUIDCreateString(allocator: Ref, uuid: Ref) -> Ref;
        fn CFRelease(object: Ref);
    }

    extern "C" {
        fn dispatch_queue_create(label: *const c_char, attributes: Ref) -> *mut c_void;
        fn dispatch_sync_f(
            queue: *mut c_void,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
        fn dispatch_release(object: *mut c_void);
    }

    #[derive(Default)]
    struct Replay {
        changes: Mutex<(HashMap<PathBuf, u32>, bool)>,
        done: Condvar,
    }

    extern "C" fn received(
        _: Stream,
        info: *mut c_void,
        count: usize,
        paths: *mut c_void,
        flags: *const u32,
        _: *const u64,
    ) {
        let replay = unsafe { &*(info as *const Replay) };
        let paths = unsafe { std::slice::from_raw_parts(paths as *const *const c_char, count) };
        let flags = unsafe { std::slice::from_raw_parts(flags, count) };
        let mut changes = replay.changes.lock().expect("replay changes");
        for (&path, &flag) in paths.iter().zip(flags) {
            if flag & HISTORY_DONE != 0 {
                changes.1 = true;
                replay.done.notify_all();
                continue;
            }
            let path = OsStr::from_bytes(unsafe { CStr::from_ptr(path) }.to_bytes());
            *changes.0.entry(PathBuf::from(path)).or_default() |= flag;
        }
    }

    extern "C" fn drained(_: *mut c_void) {}

    pub fn current_event_id() -> u64 {
        unsafe { FSEventsGetCurrentEventId() }
    }

    pub fn device_uuid(device: u64) -> Option<String> {
        unsafe {
            let uuid = FSEventsCopyUUIDForDevice(device as i32);
            if uuid.is_null() {
                return None;
            }
            let text = CFUUIDCreateString(std::ptr::null(), uuid);
            let mut buffer = [0 as c_char; 64];
            let copied = CFStringGetCString(text, buffer.as_mut_ptr(), 64, UTF8) != 0;
            CFRelease(text);
            CFRelease(uuid);
            copied.then(|| {
                CStr::from_ptr(buffer.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            })
        }
    }

    pub fn changes(root: &Path, since: u64) -> Option<HashMap<PathBuf, u32>> {
        let root = CString::new(root.as_os_str().as_bytes()).ok()?;
        let replay = Replay::default();
        unsafe {
            let path = CFStringCreateWithCString(std::ptr::null(), root.as_ptr(), UTF8);
            let paths = CFArrayCreate(std::ptr::null(), &path, 1, std::ptr::null());
            let context = Context {
                version: 0,
                info: &replay as *const Replay as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                describe: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                received,
                &context,
                paths,
                since,
                0.0,
                NO_DEFER | FILE_EVENTS,
            );
            let queue = dispatch_queue_create(c"cc-transcript-index".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            let started = FSEventStreamStart(stream) != 0;
            let finished = started && {
                let changes = replay.changes.lock().expect("replay changes");
                replay
                    .done
                    .wait_timeout_while(changes, REPLAY_WAIT, |changes| !changes.1)
                    .expect("replay changes")
                    .0
                     .1
            };
            if started {
                FSEventStreamStop(stream);
            }
            FSEventStreamInvalidate(stream);
            FSEventStreamRelease(stream);
            dispatch_sync_f(queue, std::ptr::null_mut(), drained);
            dispatch_release(queue);
            CFRelease(paths);
            CFRelease(path);
            finished.then(|| replay.changes.into_inner().expect("replay changes").0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn sizes(index: Index) -> Vec<(PathBuf, u64)> {
        index
            .records
            .into_iter()
            .map(|(path, stamp)| (path, stamp.size))
            .collect()
    }

    #[test]
    fn walks_only_transcripts_without_following_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&root.path().join("p/a.jsonl"), "a");
        write(&root.path().join("p/s/subagents/b.jsonl"), "bb");
        write(&root.path().join("p/s/subagents/b.meta.json"), "{}");
        write(&outside.path().join("secret.jsonl"), "x");
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("gone.jsonl"),
            root.path().join("p/ghost.jsonl"),
        )
        .unwrap();
        assert_eq!(
            sizes(full(&fs::canonicalize(root.path()).unwrap())),
            vec![
                (PathBuf::from("p/a.jsonl"), 1),
                (PathBuf::from("p/s/subagents/b.jsonl"), 2),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn encoded_indexes_round_trip_and_reject_truncation() {
        let index = Index {
            root: PathBuf::from("/r"),
            epoch: Some(Epoch {
                volume: "uuid".into(),
                root: 9,
                event: 42,
            }),
            records: BTreeMap::from([(
                PathBuf::from("p/a.jsonl"),
                Stamp {
                    mtime: 1.5,
                    size: 3,
                    inode: 7,
                    link: true,
                },
            )]),
        };
        let bytes = persisted::encode(&index);
        assert_eq!(persisted::decode(&bytes), Some(index));
        assert_eq!(persisted::decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(persisted::decode(b"garbage"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refreshes_follow_appends_creations_renames_replacements_and_deletions() {
        use std::io::Write;

        let append = |path: &Path, text: &str| {
            fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap()
        };
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        write(&root.path().join("p/a.jsonl"), "a");
        write(&root.path().join("p/b.jsonl"), "b");
        write(&root.path().join("p/d.jsonl/inner.jsonl"), "i");
        write(&outside.path().join("t.jsonl"), "t");
        std::os::unix::fs::symlink(
            outside.path().join("t.jsonl"),
            root.path().join("p/l.jsonl"),
        )
        .unwrap();
        assert_eq!(
            persisted::refreshed(&canonical, store.path()).records.len(),
            4
        );
        let file = persisted::index_file(store.path(), &canonical);
        assert!(persisted::decode(&fs::read(&file).unwrap())
            .unwrap()
            .epoch
            .is_some());
        append(&root.path().join("p/a.jsonl"), "ppend");
        append(&outside.path().join("t.jsonl"), "ail");
        fs::remove_file(root.path().join("p/b.jsonl")).unwrap();
        fs::remove_dir_all(root.path().join("p/d.jsonl")).unwrap();
        write(&root.path().join("p/d.jsonl"), "dd");
        write(&root.path().join("q/s/subagents/c.jsonl"), "ccc");
        fs::rename(root.path().join("q"), root.path().join("r")).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let refreshed = persisted::refreshed(&canonical, store.path());
        assert_eq!(
            sizes(refreshed),
            vec![
                (PathBuf::from("p/a.jsonl"), 6),
                (PathBuf::from("p/d.jsonl"), 2),
                (PathBuf::from("p/l.jsonl"), 4),
                (PathBuf::from("r/s/subagents/c.jsonl"), 3),
            ]
        );
        assert_eq!(
            sizes(persisted::decode(&fs::read(&file).unwrap()).unwrap()),
            sizes(full(&canonical))
        );
    }
}

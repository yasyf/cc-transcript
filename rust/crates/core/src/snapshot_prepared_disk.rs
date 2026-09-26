use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use sha2::{Digest, Sha256};
use sonic_rs::{json, Value};

use crate::snapshot::{SnapshotError, SourceStamp, Status};
use crate::snapshot_prepared::PreparedFacts;

const VERSION: &[u8; 8] = b"CTPF0001";
const HEADER_BYTES: usize = 80;
const OWNER_LOCK: &CStr = c"owner.lock";
const MAX_SCANNED_OWNERS: usize = 32;
const MAX_CLEANED_OWNERS: usize = 8;
const MAX_CLEANED_FILES: usize = 128;
const MAX_SCANNED_FILES_PER_OWNER: usize = 256;
const MAX_CLEANED_BYTES: u64 = 128 * 1024 * 1024;

static ACTIVE_OWNERS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn incomplete(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, reason)
}

fn disk_error(error: io::Error) -> SnapshotError {
    incomplete(format!("prepared facts disk cache: {error}"))
}

fn openat_file(dir_fd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<std::fs::File> {
    let fd = unsafe {
        libc::openat(
            dir_fd,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn unlinkat_file(dir_fd: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    if unsafe { libc::unlinkat(dir_fd, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn private_file(file: &std::fs::File) -> bool {
    file.metadata().is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
    })
}

fn hex_bytes(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn generated_entry(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    (bytes.len() == 70 && bytes.ends_with(b".facts") && hex_bytes(&bytes[..64]))
        || (bytes.len() >= 5
            && bytes.len() <= 20
            && bytes.ends_with(b".tmp")
            && hex_bytes(&bytes[..bytes.len() - 4]))
}

fn only_lock_remains(dir_fd: libc::c_int) -> bool {
    let Ok(mut entries) = DirEntries::new(dir_fd) else {
        return false;
    };
    while let Some(name) = entries.next() {
        if name.as_c_str() != OWNER_LOCK {
            return false;
        }
    }
    true
}

fn restore_lock(dir_fd: libc::c_int) {
    if let Ok(file) = openat_file(
        dir_fd,
        OWNER_LOCK,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    ) {
        unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
    }
}

struct DirEntries(*mut libc::DIR);

impl DirEntries {
    fn new(dir_fd: libc::c_int) -> io::Result<Self> {
        let file = openat_file(dir_fd, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
        let dir = unsafe { libc::fdopendir(file.as_raw_fd()) };
        if dir.is_null() {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(file);
        Ok(Self(dir))
    }

    fn next(&mut self) -> Option<CString> {
        loop {
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                return None;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            return Some(name.to_owned());
        }
    }
}

impl Drop for DirEntries {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

struct CleanupBudget {
    owners: usize,
    files: usize,
    bytes: u64,
    max_files: usize,
    max_bytes: u64,
}

impl CleanupBudget {
    fn new(max_files: usize, max_bytes: u64) -> Self {
        Self {
            owners: 0,
            files: 0,
            bytes: 0,
            max_files,
            max_bytes,
        }
    }

    fn exhausted(&self) -> bool {
        self.owners == MAX_CLEANED_OWNERS
            || self.files == self.max_files
            || self.bytes == self.max_bytes
    }
}

pub struct PreparedDiskKey {
    digest: [u8; 32],
}

impl PreparedDiskKey {
    pub fn new(
        stamp: SourceStamp,
        registry_generation: &str,
        admission: &str,
        authority: &Value,
        classifier: &Value,
    ) -> Result<Self, SnapshotError> {
        let binding = json!({
            "version": "prepared-facts/1",
            "source": [
                stamp.identity.device.to_string(),
                stamp.identity.inode.to_string(),
                stamp.size.to_string(),
                stamp.mtime_ns.to_string(),
                stamp.ctime_ns.to_string(),
            ],
            "registry_generation": registry_generation,
            "admission": admission,
            "authority": authority,
            "classifier": classifier,
        });
        let canonical = crate::ids::canonical_json(&binding)
            .map_err(|error| incomplete(format!("prepared facts key: {error}")))?;
        Ok(Self {
            digest: Sha256::digest(canonical.as_bytes()).into(),
        })
    }

    fn file_name(&self) -> String {
        format!("{:x}.facts", Sha256::digest(self.digest))
    }
}

pub enum DiskLookup {
    Hit(PreparedFacts),
    Miss,
    Retired,
}

pub struct DiskStats {
    pub entries: usize,
    pub bytes: usize,
    pub retired: usize,
    pub writes: u64,
    pub write_bytes: u64,
}

struct DiskEntry {
    bytes: usize,
    last_used: u64,
}

#[derive(Default)]
struct DiskState {
    entries: HashMap<[u8; 32], DiskEntry>,
    retired: usize,
    bytes: usize,
    clock: u64,
    writes: u64,
    write_bytes: u64,
}

pub struct PreparedDiskCache {
    namespace: PathBuf,
    namespace_file: std::fs::File,
    dir: PathBuf,
    dir_file: std::fs::File,
    owner_name: CString,
    _owner_lock: std::fs::File,
    dir_device: u64,
    dir_inode: u64,
    max_bytes: usize,
    state: Mutex<DiskState>,
}

impl PreparedDiskCache {
    pub fn new(owner_epoch: &str, max_bytes: usize) -> Result<Self, SnapshotError> {
        if owner_epoch.len() != 64 || !owner_epoch.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(incomplete("invalid prepared facts owner epoch"));
        }
        if max_bytes <= HEADER_BYTES {
            return Err(incomplete("prepared facts disk cache capacity exhausted"));
        }
        let namespace = fs::canonicalize(std::env::temp_dir())
            .map_err(disk_error)?
            .join("cc-transcript-prepared");
        match DirBuilder::new().mode(0o700).create(&namespace) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(disk_error(error)),
        }
        let namespace_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&namespace)
            .map_err(disk_error)?;
        let namespace_metadata = namespace_file.metadata().map_err(disk_error)?;
        if !namespace_metadata.is_dir()
            || namespace_metadata.mode() & 0o777 != 0o700
            || namespace_metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(incomplete(
                "prepared facts cache namespace is not owner-private",
            ));
        }
        let owner_name = CString::new(owner_epoch).expect("hex owner epoch");
        let dir = namespace.join(owner_epoch);
        let mut active_owners = ACTIVE_OWNERS.lock().expect("active prepared owners");
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(disk_error)?;
        let dir_file = openat_file(
            namespace_file.as_raw_fd(),
            &owner_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(disk_error)?;
        let metadata = dir_file.metadata().map_err(disk_error)?;
        if !metadata.is_dir()
            || metadata.mode() & 0o777 != 0o700
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(incomplete(
                "prepared facts cache directory is not owner-private",
            ));
        }
        let owner_lock = openat_file(
            dir_file.as_raw_fd(),
            OWNER_LOCK,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(disk_error)?;
        if unsafe { libc::fchmod(owner_lock.as_raw_fd(), 0o600) } != 0 {
            return Err(disk_error(io::Error::last_os_error()));
        }
        if unsafe { libc::flock(owner_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(disk_error(io::Error::last_os_error()));
        }
        active_owners.insert(owner_epoch.to_owned());
        drop(active_owners);
        let cache = Self {
            namespace,
            namespace_file,
            dir,
            dir_file,
            owner_name,
            _owner_lock: owner_lock,
            dir_device: metadata.dev(),
            dir_inode: metadata.ino(),
            max_bytes,
            state: Mutex::new(DiskState::default()),
        };
        cache.cleanup_stale_owners();
        Ok(cache)
    }

    fn check_dir(&self) -> Result<(), SnapshotError> {
        let namespace = fs::symlink_metadata(&self.namespace).map_err(disk_error)?;
        let pinned_namespace = self.namespace_file.metadata().map_err(disk_error)?;
        if !namespace.is_dir()
            || namespace.dev() != pinned_namespace.dev()
            || namespace.ino() != pinned_namespace.ino()
            || namespace.mode() & 0o777 != 0o700
            || namespace.uid() != unsafe { libc::geteuid() }
        {
            return Err(incomplete("prepared facts cache namespace changed"));
        }
        let metadata = fs::symlink_metadata(&self.dir).map_err(disk_error)?;
        if !metadata.is_dir()
            || metadata.dev() != self.dir_device
            || metadata.ino() != self.dir_inode
            || metadata.mode() & 0o777 != 0o700
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(incomplete("prepared facts cache directory changed"));
        }
        Ok(())
    }

    fn cleanup_stale_owners(&self) {
        self.cleanup_stale_owners_with_limits(MAX_CLEANED_FILES, MAX_CLEANED_BYTES);
    }

    fn cleanup_stale_owners_with_limits(&self, max_files: usize, max_bytes: u64) {
        let Ok(mut owners) = DirEntries::new(self.namespace_file.as_raw_fd()) else {
            return;
        };
        let mut budget = CleanupBudget::new(max_files, max_bytes);
        for _ in 0..MAX_SCANNED_OWNERS {
            if budget.exhausted() {
                break;
            }
            let Some(name) = owners.next() else {
                break;
            };
            let bytes = name.to_bytes();
            if bytes.len() != 64
                || !hex_bytes(bytes)
                || name == self.owner_name
                || ACTIVE_OWNERS
                    .lock()
                    .expect("active prepared owners")
                    .contains(std::str::from_utf8(bytes).expect("hex owner epoch"))
            {
                continue;
            }
            self.cleanup_stale_owner(&name, &mut budget);
        }
    }

    fn cleanup_stale_owner(&self, name: &CStr, budget: &mut CleanupBudget) {
        let Ok(owner) = openat_file(
            self.namespace_file.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        ) else {
            return;
        };
        let Ok(metadata) = owner.metadata() else {
            return;
        };
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
        {
            return;
        }
        let Ok(lock) = openat_file(owner.as_raw_fd(), OWNER_LOCK, libc::O_RDWR) else {
            return;
        };
        if !private_file(&lock)
            || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        {
            return;
        }
        budget.owners += 1;
        let Ok(mut entries) = DirEntries::new(owner.as_raw_fd()) else {
            return;
        };
        let mut complete = true;
        for _ in 0..MAX_SCANNED_FILES_PER_OWNER {
            let Some(entry_name) = entries.next() else {
                break;
            };
            if entry_name.as_c_str() == OWNER_LOCK {
                continue;
            }
            if !generated_entry(&entry_name) {
                complete = false;
                continue;
            }
            if budget.files == budget.max_files || budget.bytes == budget.max_bytes {
                complete = false;
                break;
            }
            let Ok(file) = openat_file(owner.as_raw_fd(), &entry_name, libc::O_RDWR) else {
                complete = false;
                continue;
            };
            if !private_file(&file) {
                complete = false;
                continue;
            }
            let Ok(file_metadata) = file.metadata() else {
                complete = false;
                continue;
            };
            let length = file_metadata.len();
            let remaining = budget.max_bytes - budget.bytes;
            if length > remaining {
                if file.set_len(length - remaining).is_ok() {
                    budget.bytes = budget.max_bytes;
                }
                complete = false;
                break;
            }
            if unlinkat_file(owner.as_raw_fd(), &entry_name, 0).is_err() {
                complete = false;
                continue;
            }
            budget.files += 1;
            budget.bytes += length;
        }
        drop(entries);
        if complete && only_lock_remains(owner.as_raw_fd()) {
            if unlinkat_file(owner.as_raw_fd(), OWNER_LOCK, 0).is_ok()
                && unlinkat_file(self.namespace_file.as_raw_fd(), name, libc::AT_REMOVEDIR).is_err()
            {
                restore_lock(owner.as_raw_fd());
            }
        }
    }

    fn entry_name(&self, digest: [u8; 32]) -> CString {
        CString::new(PreparedDiskKey { digest }.file_name()).expect("digest file name")
    }

    fn open_entry(&self, name: &CStr, flags: libc::c_int) -> io::Result<std::fs::File> {
        openat_file(self.dir_file.as_raw_fd(), name, flags)
    }

    fn unlink_entry(&self, name: &CStr) -> io::Result<()> {
        if unsafe { libc::unlinkat(self.dir_file.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn rename_entry(&self, from: &CStr, to: &CStr) -> io::Result<()> {
        if unsafe {
            libc::renameat(
                self.dir_file.as_raw_fd(),
                from.as_ptr(),
                self.dir_file.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    fn entry_path(&self, digest: [u8; 32]) -> PathBuf {
        self.dir.join(PreparedDiskKey { digest }.file_name())
    }

    fn retire(&self, state: &mut DiskState, digest: [u8; 32]) -> Result<(), SnapshotError> {
        if let Some(entry) = state.entries.get(&digest) {
            match self.unlink_entry(&self.entry_name(digest)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(disk_error(error)),
            }
            state.bytes -= entry.bytes;
            state.entries.remove(&digest);
            state.retired += 1;
        }
        Ok(())
    }

    pub fn lookup(&self, key: &PreparedDiskKey) -> Result<DiskLookup, SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        if !state.entries.contains_key(&key.digest) {
            return Ok(DiskLookup::Miss);
        }
        let result = (|| -> Result<PreparedFacts, SnapshotError> {
            let file = self
                .open_entry(&self.entry_name(key.digest), libc::O_RDONLY)
                .map_err(disk_error)?;
            let metadata = file.metadata().map_err(disk_error)?;
            let expected = state.entries.get(&key.digest).expect("cached entry").bytes;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o600
                || metadata.nlink() != 1
                || metadata.len() != expected as u64
            {
                return Err(incomplete("prepared facts cache entry changed"));
            }
            let mut bytes = Vec::with_capacity(expected);
            file.take(expected.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .map_err(disk_error)?;
            if bytes.len() != expected
                || bytes[..8] != *VERSION
                || bytes[8..40] != key.digest
                || u64::from_le_bytes(bytes[40..48].try_into().expect("length header"))
                    != (expected - HEADER_BYTES) as u64
                || Sha256::digest(&bytes[HEADER_BYTES..]).as_slice() != &bytes[48..80]
            {
                return Err(incomplete("prepared facts cache integrity check failed"));
            }
            let mut facts: PreparedFacts = sonic_rs::from_slice(&bytes[HEADER_BYTES..])
                .map_err(|_| incomplete("prepared facts cache payload invalid"))?;
            facts.refresh_accounted();
            Ok(facts)
        })();
        match result {
            Ok(facts) => {
                state.clock += 1;
                let clock = state.clock;
                state
                    .entries
                    .get_mut(&key.digest)
                    .expect("cached entry")
                    .last_used = clock;
                Ok(DiskLookup::Hit(facts))
            }
            Err(_) => {
                self.retire(&mut state, key.digest)?;
                Ok(DiskLookup::Retired)
            }
        }
    }

    pub fn has_entry(&self, key: &PreparedDiskKey) -> Result<bool, SnapshotError> {
        self.check_dir()?;
        Ok(self
            .state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .contains_key(&key.digest))
    }

    pub fn insert(
        &self,
        key: &PreparedDiskKey,
        facts: &PreparedFacts,
    ) -> Result<(), SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        if state.entries.contains_key(&key.digest) {
            return Ok(());
        }
        let mut payload = BoundedVec {
            bytes: Vec::new(),
            limit: self.max_bytes - HEADER_BYTES,
        };
        sonic_rs::to_writer(sonic_rs::writer::BufferedWriter::new(&mut payload), facts)
            .map_err(|_| incomplete("prepared facts disk cache capacity exhausted"))?;
        let size = HEADER_BYTES + payload.bytes.len();
        while size > self.max_bytes.saturating_sub(state.bytes) {
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(digest, _)| *digest)
                .ok_or_else(|| incomplete("prepared facts disk cache capacity exhausted"))?;
            self.retire(&mut state, oldest)?;
        }
        state.clock += 1;
        let temp = CString::new(format!("{:x}.tmp", state.clock)).expect("temp file name");
        let result = (|| -> Result<(), SnapshotError> {
            let mut file = self
                .open_entry(&temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
                .map_err(disk_error)?;
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(disk_error(io::Error::last_os_error()));
            }
            file.write_all(VERSION).map_err(disk_error)?;
            file.write_all(&key.digest).map_err(disk_error)?;
            file.write_all(&(payload.bytes.len() as u64).to_le_bytes())
                .map_err(disk_error)?;
            file.write_all(&Sha256::digest(&payload.bytes))
                .map_err(disk_error)?;
            file.write_all(&payload.bytes).map_err(disk_error)?;
            drop(file);
            self.rename_entry(&temp, &self.entry_name(key.digest))
                .map_err(disk_error)
        })();
        if result.is_err() {
            let _ = self.unlink_entry(&temp);
            return result;
        }
        state.bytes += size;
        state.writes += 1;
        state.write_bytes += size as u64;
        let clock = state.clock;
        state.entries.insert(
            key.digest,
            DiskEntry {
                bytes: size,
                last_used: clock,
            },
        );
        Ok(())
    }

    pub fn stats(&self) -> DiskStats {
        let state = self.state.lock().expect("prepared facts disk state");
        DiskStats {
            entries: state.entries.len(),
            bytes: state.bytes,
            retired: state.retired,
            writes: state.writes,
            write_bytes: state.write_bytes,
        }
    }
}

impl Drop for PreparedDiskCache {
    fn drop(&mut self) {
        if self.check_dir().is_ok() {
            if let Ok(state) = self.state.get_mut() {
                let mut files = 0;
                let mut bytes = 0;
                for digest in state.entries.keys() {
                    if files == MAX_CLEANED_FILES || bytes == MAX_CLEANED_BYTES {
                        break;
                    }
                    let name = CString::new(PreparedDiskKey { digest: *digest }.file_name())
                        .expect("digest file name");
                    let Ok(file) = openat_file(self.dir_file.as_raw_fd(), &name, libc::O_RDWR)
                    else {
                        continue;
                    };
                    if !private_file(&file) {
                        continue;
                    }
                    let Ok(metadata) = file.metadata() else {
                        continue;
                    };
                    let remaining = MAX_CLEANED_BYTES - bytes;
                    if metadata.len() > remaining {
                        let _ = file.set_len(metadata.len() - remaining);
                        break;
                    }
                    if unlinkat_file(self.dir_file.as_raw_fd(), &name, 0).is_ok() {
                        files += 1;
                        bytes += metadata.len();
                    }
                }
            }
            if only_lock_remains(self.dir_file.as_raw_fd())
                && self.unlink_entry(OWNER_LOCK).is_ok()
                && unlinkat_file(
                    self.namespace_file.as_raw_fd(),
                    &self.owner_name,
                    libc::AT_REMOVEDIR,
                )
                .is_err()
            {
                restore_lock(self.dir_file.as_raw_fd());
            }
        }
        ACTIVE_OWNERS
            .lock()
            .expect("active prepared owners")
            .remove(self.owner_name.to_str().expect("hex owner epoch"));
    }
}

struct BoundedVec {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedVec {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "prepared facts disk cache capacity exhausted",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use sonic_rs::json;

    use super::*;
    use crate::snapshot::SourceIdentity;
    use crate::snapshot_prepared::OverrideEvent;

    static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

    fn cache(cap: usize) -> PreparedDiskCache {
        let id = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        let epoch = format!("{:032x}{:032x}", std::process::id(), id);
        PreparedDiskCache::new(&epoch, cap).unwrap()
    }

    fn stamp(size: u64) -> SourceStamp {
        SourceStamp {
            identity: SourceIdentity {
                device: 10,
                inode: 20,
            },
            size,
            mtime_ns: 30,
            ctime_ns: 40,
        }
    }

    fn key(stamp: SourceStamp, authority: &Value) -> PreparedDiskKey {
        PreparedDiskKey::new(
            stamp,
            "registry-1",
            "hook",
            authority,
            &json!({"id":"native","version":"1"}),
        )
        .unwrap()
    }

    fn facts() -> PreparedFacts {
        PreparedFacts {
            inputs: json!({
                "calls": [["Read", ["/repo/file.rs"]]],
                "commands": [],
                "edited_files": [],
                "skills": [],
            }),
            has_error: false,
            override_events: Some(vec![OverrideEvent {
                text: "override only in source".to_owned(),
                tools: vec!["Read".to_owned()],
            }]),
            accounted: 2048,
        }
    }

    fn stale_owner(cache: &PreparedDiskCache, bytes: usize) -> (PathBuf, PathBuf) {
        let id = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        let dir = cache.namespace.join(format!(
            "{:032x}{:032x}",
            u64::MAX - std::process::id() as u64,
            u64::MAX - id
        ));
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("owner.lock"))
            .unwrap();
        assert_eq!(lock.metadata().unwrap().mode() & 0o777, 0o600);
        drop(lock);
        let file_path = dir.join(format!("{:064x}.facts", id));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file_path)
            .unwrap();
        file.write_all(&vec![1; bytes]).unwrap();
        (dir, file_path)
    }

    #[test]
    fn stores_revision_bound_facts_in_owner_private_files() {
        let cache = cache(4096);
        let first = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&first, &facts()).unwrap();
        assert_eq!(cache.stats().entries, 1);
        assert!(cache.stats().bytes <= 4096);
        assert_eq!(cache.stats().writes, 1);
        assert_eq!(cache.stats().write_bytes, cache.stats().bytes as u64);
        let dir = fs::metadata(&cache.dir).unwrap();
        let file = fs::metadata(cache.entry_path(first.digest)).unwrap();
        assert_eq!(dir.mode() & 0o777, 0o700);
        assert_eq!(file.mode() & 0o777, 0o600);
        match cache.lookup(&first).unwrap() {
            DiskLookup::Hit(loaded) => {
                assert!(loaded.accounted_bytes() >= std::mem::size_of::<PreparedFacts>());
                assert_eq!(loaded.inputs, facts().inputs);
                assert_eq!(
                    loaded.override_events.unwrap()[0].text,
                    "override only in source"
                );
            }
            _ => panic!("expected completed facts"),
        }
        cache.insert(&first, &facts()).unwrap();
        assert_eq!(cache.stats().writes, 1);
        assert!(matches!(
            cache
                .lookup(&key(stamp(2), &json!({"root":"/repo"})))
                .unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            cache
                .lookup(&key(stamp(1), &json!({"root":"/elsewhere"})))
                .unwrap(),
            DiskLookup::Miss
        ));
        for changed in [
            PreparedDiskKey::new(
                stamp(1),
                "registry-2",
                "hook",
                &json!({"root":"/repo"}),
                &json!({"id":"native","version":"1"}),
            )
            .unwrap(),
            PreparedDiskKey::new(
                stamp(1),
                "registry-1",
                "review",
                &json!({"root":"/repo"}),
                &json!({"id":"native","version":"1"}),
            )
            .unwrap(),
            PreparedDiskKey::new(
                stamp(1),
                "registry-1",
                "hook",
                &json!({"root":"/repo"}),
                &json!({"id":"native","version":"2"}),
            )
            .unwrap(),
        ] {
            assert!(matches!(cache.lookup(&changed).unwrap(), DiskLookup::Miss));
        }
    }

    #[test]
    fn evicted_revision_can_rebuild_in_a_later_event() {
        let sample = facts();
        let cap = HEADER_BYTES + sonic_rs::to_vec(&sample).unwrap().len();
        let cache = cache(cap);
        let first = key(stamp(1), &json!({"root":"/repo"}));
        let second = key(stamp(2), &json!({"root":"/repo"}));
        cache.insert(&first, &sample).unwrap();
        cache.insert(&second, &sample).unwrap();
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().retired, 1);
        assert_eq!(cache.stats().writes, 2);
        assert_eq!(cache.stats().write_bytes, (2 * cap) as u64);
        assert!(matches!(cache.lookup(&first).unwrap(), DiskLookup::Miss));
        assert!(matches!(cache.lookup(&second).unwrap(), DiskLookup::Hit(_)));
        cache.insert(&first, &sample).unwrap();
        assert_eq!(cache.stats().writes, 3);
        assert_eq!(cache.stats().retired, 2);
        assert!(matches!(cache.lookup(&first).unwrap(), DiskLookup::Hit(_)));
        assert!(matches!(cache.lookup(&second).unwrap(), DiskLookup::Miss));
    }

    #[test]
    fn corrupted_entry_cannot_answer_a_query() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts()).unwrap();
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Retired));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Miss));
        cache.insert(&entry, &facts()).unwrap();
        assert_eq!(cache.stats().writes, 2);
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Hit(_)));
    }

    #[test]
    fn oversized_facts_return_incomplete_without_an_entry() {
        let cache = cache(HEADER_BYTES + 1);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert_eq!(
            cache.insert(&entry, &facts()).unwrap_err().status,
            Status::Incomplete
        );
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Miss));
    }

    #[test]
    fn repeated_evictions_stay_bounded_and_rebuildable() {
        let sample = facts();
        let cap = HEADER_BYTES + sonic_rs::to_vec(&sample).unwrap().len();
        let cache = cache(cap);
        for revision in 0..256 {
            cache
                .insert(&key(stamp(revision), &json!({"root":"/repo"})), &sample)
                .unwrap();
        }
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().bytes, cap);
        assert_eq!(cache.stats().retired, 255);
        assert_eq!(cache.stats().writes, 256);
        assert!(matches!(
            cache
                .lookup(&key(stamp(0), &json!({"root":"/repo"})))
                .unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn owner_restart_does_not_reuse_previous_owner_files() {
        let first = cache(4096);
        let old_dir = first.dir.clone();
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        first.insert(&entry, &facts()).unwrap();
        drop(first);
        assert!(!old_dir.exists());
        let restarted = cache(4096);
        assert!(matches!(
            restarted.lookup(&entry).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn startup_cleanup_skips_active_owner() {
        let active = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        active.insert(&entry, &facts()).unwrap();
        let cleaner = cache(4096);
        cleaner.cleanup_stale_owners_with_limits(128, 128 * 1024 * 1024);
        assert!(active.dir.exists());
        assert!(matches!(active.lookup(&entry).unwrap(), DiskLookup::Hit(_)));
    }

    #[test]
    fn startup_cleanup_truncates_stale_files_within_byte_budget() {
        let cleaner = cache(4096);
        let (dir, file_path) = stale_owner(&cleaner, 50);
        for remaining in [34, 18, 2] {
            cleaner.cleanup_stale_owners_with_limits(128, 16);
            assert_eq!(fs::metadata(&file_path).unwrap().len(), remaining);
            assert!(dir.exists());
        }
        cleaner.cleanup_stale_owners_with_limits(128, 16);
        assert!(!dir.exists());
    }

    #[test]
    fn replaced_entry_symlink_cannot_escape_private_directory() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts()).unwrap();
        let path = cache.entry_path(entry.digest);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Retired));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(cache.lookup(&entry).unwrap(), DiskLookup::Miss));
    }
}

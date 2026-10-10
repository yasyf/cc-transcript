#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use sha2::{Digest, Sha256};
use sonic_rs::{json, Value};

use crate::snapshot::{SnapshotError, SourceStamp, Status};
#[cfg(test)]
use crate::snapshot_ledger::Reserved;
use crate::snapshot_ledger::{Table, Work};
use crate::snapshot_memory::sonic_node_buffer_bytes;
use crate::snapshot_prepared::PreparedFacts;

const VERSION: &[u8; 8] = b"CTPF0001";
pub(crate) const HEADER_BYTES: usize = 80;
const SONIC_STRING_BLOCK_LANES: usize = 32;
const OWNER_LOCK: &CStr = c"owner.lock";
const STAGING: &CStr = c"staging";
const MAX_SCANNED_OWNERS: usize = 32;
const MAX_CLEANED_OWNERS: usize = 8;
const MAX_CLEANED_FILES: usize = 128;
const MAX_SCANNED_FILES_PER_OWNER: usize = 256;
const MAX_CLEANED_BYTES: u64 = 128 * 1024 * 1024;

static ACTIVE_OWNERS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishStep {
    Staged,
    Created,
    Renamed,
}

#[cfg(test)]
type PublishHook = Box<dyn FnMut(PublishStep) + Send>;

#[cfg(test)]
static PUBLISH_HOOKS: LazyLock<Mutex<HashMap<CString, PublishHook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn run_publish_hook(owner_name: &CStr, step: PublishStep) {
    if let Some(hook) = PUBLISH_HOOKS
        .lock()
        .expect("publish hooks")
        .get_mut(owner_name)
    {
        hook(step);
    }
}

#[cfg(test)]
pub(crate) type ReadHook = Box<dyn FnMut() + Send>;

#[cfg(test)]
static READ_HOOKS: LazyLock<Mutex<HashMap<CString, ReadHook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn run_read_hook(owner_name: &CStr) {
    if let Some(hook) = READ_HOOKS.lock().expect("read hooks").get_mut(owner_name) {
        hook();
    }
}

fn namespace_path() -> io::Result<PathBuf> {
    Ok(fs::canonicalize(std::env::temp_dir())?.join("cc-transcript-prepared"))
}

fn incomplete(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, reason)
}

fn disk_error(error: io::Error) -> SnapshotError {
    incomplete(format!("prepared facts disk cache: {error}"))
}

fn decode_transient_bytes(payload: usize) -> usize {
    sonic_node_buffer_bytes(payload) + 2 * (payload + SONIC_STRING_BLOCK_LANES)
}

fn probed_read_bytes(stored: usize) -> usize {
    stored.saturating_add(1)
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

fn mkdirat_private(dir_fd: libc::c_int, name: &CStr) -> io::Result<()> {
    if unsafe { libc::mkdirat(dir_fd, name.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn renameat_file(
    from_fd: libc::c_int,
    from: &CStr,
    to_fd: libc::c_int,
    to: &CStr,
) -> io::Result<()> {
    if unsafe { libc::renameat(from_fd, from.as_ptr(), to_fd, to.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

type EntryIdentity = (libc::dev_t, libc::ino_t);

fn entry_identity(dir_fd: libc::c_int, name: &CStr) -> io::Result<EntryIdentity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

fn file_identity(file: &std::fs::File) -> io::Result<EntryIdentity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino))
}

fn unlink_owned_file(dir_fd: libc::c_int, name: &CStr, owned: &std::fs::File) {
    let Ok(owned) = file_identity(owned) else {
        return;
    };
    if entry_identity(dir_fd, name).is_ok_and(|identity| identity == owned) {
        let _ = unlinkat_file(dir_fd, name, 0);
    }
}

fn open_private_dir(dir_fd: libc::c_int, name: &CStr) -> io::Result<(std::fs::File, fs::Metadata)> {
    let file = openat_file(dir_fd, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let metadata = file.metadata()?;
    Ok((file, metadata))
}

fn private_dir(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o777 == 0o700
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
                stamp.identity.window_base.to_string(),
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

#[derive(Default)]
pub struct DiskRead {
    pub operations: u64,
    pub bytes: u64,
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
    accounted: usize,
    last_used: u64,
}

struct DiskState {
    entries: Table<[u8; 32], DiskEntry>,
    retired: usize,
    bytes: usize,
    clock: u64,
    writes: u64,
    write_bytes: u64,
}

impl DiskState {
    fn new(work: Work) -> Self {
        Self {
            entries: Table::new(work),
            retired: 0,
            bytes: 0,
            clock: 0,
            writes: 0,
            write_bytes: 0,
        }
    }
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
    pub fn new(owner_epoch: &str, max_bytes: usize, work: Work) -> Result<Self, SnapshotError> {
        Self::open(
            namespace_path().map_err(disk_error)?,
            owner_epoch,
            max_bytes,
            work,
        )
    }

    fn open(
        namespace: PathBuf,
        owner_epoch: &str,
        max_bytes: usize,
        work: Work,
    ) -> Result<Self, SnapshotError> {
        if owner_epoch.len() != 64 || !owner_epoch.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(incomplete("invalid prepared facts owner epoch"));
        }
        if max_bytes <= HEADER_BYTES {
            return Err(incomplete("prepared facts disk cache capacity exhausted"));
        }
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
        if !private_dir(&namespace_metadata) {
            return Err(incomplete(
                "prepared facts cache namespace is not owner-private",
            ));
        }
        let staging_file = Self::open_staging(&namespace_file)?;
        let owner_name = CString::new(owner_epoch).expect("hex owner epoch");
        let staged_name = CString::new(format!("{owner_epoch}.lock")).expect("hex owner epoch");
        let dir = namespace.join(owner_epoch);
        let mut active_owners = ACTIVE_OWNERS.lock().expect("active prepared owners");
        let owner_lock = openat_file(
            staging_file.as_raw_fd(),
            &staged_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(disk_error)?;
        #[cfg(test)]
        run_publish_hook(&owner_name, PublishStep::Staged);
        let published = Self::publish_locked_owner(
            &namespace_file,
            &staging_file,
            &staged_name,
            &owner_name,
            &owner_lock,
        );
        let (dir_file, metadata) = match published {
            Ok(published) => published,
            Err(error) => {
                unlink_owned_file(staging_file.as_raw_fd(), &staged_name, &owner_lock);
                return Err(error);
            }
        };
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
            state: Mutex::new(DiskState::new(work)),
        };
        cache.cleanup_stale_owners();
        Ok(cache)
    }

    fn open_staging(namespace_file: &std::fs::File) -> Result<std::fs::File, SnapshotError> {
        match mkdirat_private(namespace_file.as_raw_fd(), STAGING) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(disk_error(error)),
        }
        let (staging_file, metadata) =
            open_private_dir(namespace_file.as_raw_fd(), STAGING).map_err(disk_error)?;
        if !private_dir(&metadata) {
            return Err(incomplete(
                "prepared facts cache staging directory is not owner-private",
            ));
        }
        Ok(staging_file)
    }

    fn publish_locked_owner(
        namespace_file: &std::fs::File,
        staging_file: &std::fs::File,
        staged_name: &CStr,
        owner_name: &CStr,
        owner_lock: &std::fs::File,
    ) -> Result<(std::fs::File, fs::Metadata), SnapshotError> {
        if unsafe { libc::fchmod(owner_lock.as_raw_fd(), 0o600) } != 0 {
            return Err(disk_error(io::Error::last_os_error()));
        }
        if unsafe { libc::flock(owner_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(disk_error(io::Error::last_os_error()));
        }
        mkdirat_private(namespace_file.as_raw_fd(), owner_name).map_err(disk_error)?;
        let renamed = Self::rename_owner_lock(
            namespace_file,
            staging_file,
            staged_name,
            owner_name,
            owner_lock,
        );
        if renamed.is_err() {
            let _ = unlinkat_file(namespace_file.as_raw_fd(), owner_name, libc::AT_REMOVEDIR);
        }
        renamed
    }

    fn rename_owner_lock(
        namespace_file: &std::fs::File,
        staging_file: &std::fs::File,
        staged_name: &CStr,
        owner_name: &CStr,
        owner_lock: &std::fs::File,
    ) -> Result<(std::fs::File, fs::Metadata), SnapshotError> {
        let (dir_file, metadata) =
            open_private_dir(namespace_file.as_raw_fd(), owner_name).map_err(disk_error)?;
        if !private_dir(&metadata) {
            return Err(incomplete(
                "prepared facts cache directory is not owner-private",
            ));
        }
        #[cfg(test)]
        run_publish_hook(owner_name, PublishStep::Created);
        if let Err(error) = renameat_file(
            staging_file.as_raw_fd(),
            staged_name,
            dir_file.as_raw_fd(),
            OWNER_LOCK,
        ) {
            unlink_owned_file(dir_file.as_raw_fd(), OWNER_LOCK, owner_lock);
            return Err(disk_error(error));
        }
        #[cfg(test)]
        run_publish_hook(owner_name, PublishStep::Renamed);
        Ok((dir_file, metadata))
    }

    pub fn check_dir(&self) -> Result<(), SnapshotError> {
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
        if !private_dir(&metadata) {
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
        renameat_file(
            self.dir_file.as_raw_fd(),
            from,
            self.dir_file.as_raw_fd(),
            to,
        )
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

    pub fn lookup(
        &self,
        key: &PreparedDiskKey,
        read: &mut DiskRead,
    ) -> Result<DiskLookup, SnapshotError> {
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
            #[cfg(test)]
            run_read_hook(&self.owner_name);
            let mut bytes = Vec::with_capacity(expected);
            read.operations += 1;
            let finished = file
                .take(probed_read_bytes(expected) as u64)
                .read_to_end(&mut bytes);
            read.bytes += bytes.len() as u64;
            finished.map_err(disk_error)?;
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

    pub fn decoded_bytes(&self, key: &PreparedDiskKey) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.digest)
            .map(|entry| {
                entry.bytes + entry.accounted + decode_transient_bytes(entry.bytes - HEADER_BYTES)
            })
    }

    pub fn read_bound(&self, key: &PreparedDiskKey) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.digest)
            .map(|entry| probed_read_bytes(entry.bytes))
    }

    #[cfg(test)]
    pub(crate) fn audit_index_bytes(&self) -> usize {
        let state = self.state.lock().expect("prepared facts disk state");
        state.entries.audit_reserved();
        state.entries.reserved_bytes()
    }

    #[cfg(test)]
    pub(crate) fn index_capacity_bytes(&self) -> usize {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .capacity()
            * size_of::<([u8; 32], DiskEntry)>()
    }

    #[cfg(test)]
    pub(crate) fn entry_file(&self, key: &PreparedDiskKey) -> PathBuf {
        self.entry_path(key.digest)
    }

    #[cfg(test)]
    pub(crate) fn set_read_hook(&self, hook: Option<ReadHook>) {
        let mut hooks = READ_HOOKS.lock().expect("read hooks");
        match hook {
            Some(hook) => hooks.insert(self.owner_name.clone(), hook),
            None => hooks.remove(&self.owner_name),
        };
    }

    #[cfg(test)]
    pub(crate) fn entry_file_len(&self, key: &PreparedDiskKey) -> u64 {
        fs::metadata(self.entry_file(key))
            .expect("cached entry file")
            .len()
    }

    pub fn insert(
        &self,
        key: &PreparedDiskKey,
        facts: &PreparedFacts,
        reserve: impl FnOnce(usize) -> bool,
        admit: impl FnOnce(usize) -> bool,
    ) -> Result<bool, SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        if state.entries.contains_key(&key.digest) {
            return Ok(true);
        }
        let limit = self.max_bytes - HEADER_BYTES;
        let mut counted = crate::snapshot_codec::Counter { bytes: 0, limit };
        crate::snapshot_codec::write_json(&mut counted, facts, limit)
            .map_err(|_| incomplete("prepared facts disk cache capacity exhausted"))?;
        if !reserve(counted.bytes) {
            return Ok(false);
        }
        let mut payload = Vec::with_capacity(counted.bytes);
        crate::snapshot_codec::write_json(&mut payload, facts, counted.bytes)
            .expect("counted prepared facts payload");
        assert_eq!(
            (payload.len(), payload.capacity()),
            (counted.bytes, counted.bytes),
            "prepared facts payload outgrew its count"
        );
        let size = HEADER_BYTES + payload.len();
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
            file.write_all(&(payload.len() as u64).to_le_bytes())
                .map_err(disk_error)?;
            file.write_all(&Sha256::digest(&payload))
                .map_err(disk_error)?;
            file.write_all(&payload).map_err(disk_error)?;
            drop(file);
            self.rename_entry(&temp, &self.entry_name(key.digest))
                .map_err(disk_error)
        })();
        if let Err(error) = result {
            let _ = self.unlink_entry(&temp);
            return Err(error);
        }
        if !admit(state.entries.growth_for(&key.digest)) {
            let _ = self.unlink_entry(&self.entry_name(key.digest));
            return Ok(false);
        }
        state.bytes += size;
        state.writes += 1;
        state.write_bytes += size as u64;
        let clock = state.clock;
        state.entries.reserve_for(&key.digest);
        state.entries.insert(
            key.digest,
            DiskEntry {
                bytes: size,
                accounted: facts.accounted_bytes(),
                last_used: clock,
            },
        );
        Ok(true)
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use sonic_rs::json;

    use super::*;
    use crate::snapshot::SourceIdentity;
    use crate::snapshot_prepared::OverrideEvent;

    static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

    fn epoch() -> String {
        let id = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        format!("{:032x}{:032x}", std::process::id(), id)
    }

    fn cache(cap: usize) -> PreparedDiskCache {
        PreparedDiskCache::new(&epoch(), cap, Work::default()).unwrap()
    }

    struct ForeignScan {
        found_owner: bool,
        opened_lock: bool,
        lock: Option<std::fs::File>,
        deleted: bool,
    }

    fn foreign_scan(namespace: &Path, epoch: &str) -> ForeignScan {
        let namespace_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(namespace)
            .unwrap();
        let mut scan = ForeignScan {
            found_owner: false,
            opened_lock: false,
            lock: None,
            deleted: false,
        };
        let mut owners = DirEntries::new(namespace_file.as_raw_fd()).unwrap();
        while let Some(name) = owners.next() {
            let bytes = name.to_bytes();
            if bytes.len() != 64 || !hex_bytes(bytes) || bytes != epoch.as_bytes() {
                continue;
            }
            scan.found_owner = true;
            let owner = openat_file(
                namespace_file.as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .unwrap();
            let Ok(lock) = openat_file(owner.as_raw_fd(), OWNER_LOCK, libc::O_RDWR) else {
                continue;
            };
            scan.opened_lock = true;
            if !private_file(&lock)
                || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
            {
                continue;
            }
            if only_lock_remains(owner.as_raw_fd()) {
                unlinkat_file(owner.as_raw_fd(), OWNER_LOCK, 0).unwrap();
                unlinkat_file(namespace_file.as_raw_fd(), &name, libc::AT_REMOVEDIR).unwrap();
                scan.deleted = true;
            }
            scan.lock = Some(lock);
        }
        scan
    }

    fn isolated_namespace() -> PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("cc-transcript-prepared-{}", epoch()))
    }

    fn abandoned_staging_lock(namespace: &Path, id: u64) {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(namespace.join("staging").join(format!("{id:064x}.lock")))
            .unwrap();
    }

    fn foreign_flock_errno(lock: &std::fs::File) -> Option<i32> {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return None;
        }
        io::Error::last_os_error().raw_os_error()
    }

    fn stamp(size: u64) -> SourceStamp {
        SourceStamp {
            identity: SourceIdentity {
                device: 10,
                inode: 20,
                window_base: 0,
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
        cache.insert(&first, &facts(), |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().entries, 1);
        assert!(cache.stats().bytes <= 4096);
        assert_eq!(cache.stats().writes, 1);
        assert_eq!(cache.stats().write_bytes, cache.stats().bytes as u64);
        let dir = fs::metadata(&cache.dir).unwrap();
        let file = fs::metadata(cache.entry_path(first.digest)).unwrap();
        assert_eq!(dir.mode() & 0o777, 0o700);
        assert_eq!(file.mode() & 0o777, 0o600);
        match cache.lookup(&first, &mut DiskRead::default()).unwrap() {
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
        cache.insert(&first, &facts(), |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 1);
        assert!(matches!(
            cache
                .lookup(
                    &key(stamp(2), &json!({"root":"/repo"})),
                    &mut DiskRead::default()
                )
                .unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            cache
                .lookup(
                    &key(stamp(1), &json!({"root":"/elsewhere"})),
                    &mut DiskRead::default()
                )
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
            assert!(matches!(
                cache.lookup(&changed, &mut DiskRead::default()).unwrap(),
                DiskLookup::Miss
            ));
        }
    }

    #[test]
    fn evicted_revision_can_rebuild_in_a_later_event() {
        let sample = facts();
        let cap = HEADER_BYTES + sonic_rs::to_vec(&sample).unwrap().len();
        let cache = cache(cap);
        let first = key(stamp(1), &json!({"root":"/repo"}));
        let second = key(stamp(2), &json!({"root":"/repo"}));
        cache.insert(&first, &sample, |_| true, |_| true).unwrap();
        cache.insert(&second, &sample, |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().retired, 1);
        assert_eq!(cache.stats().writes, 2);
        assert_eq!(cache.stats().write_bytes, (2 * cap) as u64);
        assert!(matches!(
            cache.lookup(&first, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            cache.lookup(&second, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        cache.insert(&first, &sample, |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 3);
        assert_eq!(cache.stats().retired, 2);
        assert!(matches!(
            cache.lookup(&first, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(matches!(
            cache.lookup(&second, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn corrupted_entry_cannot_answer_a_query() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 2);
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
    }

    #[test]
    fn oversized_facts_return_incomplete_without_an_entry() {
        let cache = cache(HEADER_BYTES + 1);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert_eq!(
            cache
                .insert(&entry, &facts(), |_| true, |_| true)
                .unwrap_err()
                .status,
            Status::Incomplete
        );
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn repeated_evictions_stay_bounded_and_rebuildable() {
        let sample = facts();
        let cap = HEADER_BYTES + sonic_rs::to_vec(&sample).unwrap().len();
        let cache = cache(cap);
        for revision in 0..256 {
            cache
                .insert(
                    &key(stamp(revision), &json!({"root":"/repo"})),
                    &sample,
                    |_| true,
                    |_| true,
                )
                .unwrap();
        }
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.stats().bytes, cap);
        assert_eq!(cache.stats().retired, 255);
        assert_eq!(cache.stats().writes, 256);
        assert!(matches!(
            cache
                .lookup(
                    &key(stamp(0), &json!({"root":"/repo"})),
                    &mut DiskRead::default()
                )
                .unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn owner_restart_does_not_reuse_previous_owner_files() {
        let first = cache(4096);
        let old_dir = first.dir.clone();
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        first.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        drop(first);
        assert!(!old_dir.exists());
        let restarted = cache(4096);
        assert!(matches!(
            restarted.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn foreign_cleanup_at_every_publication_step_cannot_unlock_or_delete_the_owner() {
        let epoch = epoch();
        let namespace = namespace_path().unwrap();
        let scans = Arc::new(Mutex::new(Vec::<(PublishStep, ForeignScan)>::new()));
        let hook = {
            let scans = scans.clone();
            let epoch = epoch.clone();
            let namespace = namespace.clone();
            Box::new(move |step: PublishStep| {
                scans
                    .lock()
                    .unwrap()
                    .push((step, foreign_scan(&namespace, &epoch)));
            })
        };
        let owner_name = CString::new(epoch.clone()).unwrap();
        PUBLISH_HOOKS
            .lock()
            .unwrap()
            .insert(owner_name.clone(), hook);
        let cache = PreparedDiskCache::new(&epoch, 4096, Work::default()).unwrap();
        PUBLISH_HOOKS.lock().unwrap().remove(&owner_name);
        let scans = std::mem::take(&mut *scans.lock().unwrap());
        let steps: Vec<PublishStep> = scans.iter().map(|(step, _)| *step).collect();
        assert_eq!(
            steps,
            [
                PublishStep::Staged,
                PublishStep::Created,
                PublishStep::Renamed
            ]
        );
        for (step, scan) in &scans {
            assert_eq!(*step != PublishStep::Staged, scan.found_owner, "{step:?}");
            assert_eq!(*step == PublishStep::Renamed, scan.opened_lock, "{step:?}");
            assert!(scan.lock.is_none(), "{step:?}");
            assert!(!scan.deleted, "{step:?}");
        }
        assert!(cache.dir.is_dir());
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(cache.dir.join("owner.lock"))
            .unwrap();
        assert!(private_file(&lock));
        assert_eq!(lock.metadata().unwrap().nlink(), 1);
        assert_eq!(foreign_flock_errno(&lock), Some(libc::EWOULDBLOCK));
        assert_eq!(
            fs::symlink_metadata(namespace.join("staging").join(format!("{epoch}.lock")))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
    }

    #[test]
    fn failed_rename_removes_only_the_owned_paths() {
        let namespace = isolated_namespace();
        let epoch = epoch();
        let foreign = namespace.join(&epoch).join("owner.lock");
        let hook = {
            let foreign = foreign.clone();
            Box::new(move |step: PublishStep| {
                if step == PublishStep::Created {
                    DirBuilder::new().mode(0o700).create(&foreign).unwrap();
                }
            })
        };
        let owner_name = CString::new(epoch.clone()).unwrap();
        PUBLISH_HOOKS
            .lock()
            .unwrap()
            .insert(owner_name.clone(), hook);
        let result = PreparedDiskCache::open(namespace.clone(), &epoch, 4096, Work::default());
        PUBLISH_HOOKS.lock().unwrap().remove(&owner_name);
        let Err(error) = result else {
            panic!("rename onto a foreign owner.lock succeeded");
        };
        assert_eq!(error.status, Status::Incomplete);
        assert!(error.reason.contains("Is a directory"), "{}", error.reason);
        assert!(fs::symlink_metadata(&foreign).unwrap().is_dir());
        assert_eq!(
            fs::symlink_metadata(namespace.join("staging").join(format!("{epoch}.lock")))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(!ACTIVE_OWNERS.lock().unwrap().contains(&epoch));
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn failed_publication_removes_only_the_staged_lock() {
        let namespace = cache(4096).namespace.clone();
        let epoch = epoch();
        let conflicting = namespace.join(&epoch);
        DirBuilder::new().mode(0o700).create(&conflicting).unwrap();
        let before = fs::metadata(&conflicting).unwrap();
        let Err(error) = PreparedDiskCache::new(&epoch, 4096, Work::default()) else {
            panic!("publication onto an existing owner succeeded");
        };
        assert_eq!(error.status, Status::Incomplete);
        assert!(error.reason.contains("File exists"), "{}", error.reason);
        assert_eq!(
            fs::symlink_metadata(namespace.join("staging").join(format!("{epoch}.lock")))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let after = fs::metadata(&conflicting).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert!(fs::read_dir(&conflicting).unwrap().next().is_none());
        assert!(!ACTIVE_OWNERS.lock().unwrap().contains(&epoch));
        fs::remove_dir(&conflicting).unwrap();
    }

    #[test]
    fn abandoned_staging_locks_do_not_starve_stale_owner_cleanup() {
        let namespace = isolated_namespace();
        let cleaner =
            PreparedDiskCache::open(namespace.clone(), &epoch(), 4096, Work::default()).unwrap();
        for id in 0..MAX_SCANNED_OWNERS as u64 + 8 {
            abandoned_staging_lock(&namespace, id);
        }
        let (dir, file_path) = stale_owner(&cleaner, 50);
        cleaner.cleanup_stale_owners_with_limits(128, 128 * 1024 * 1024);
        assert!(!file_path.exists());
        assert!(!dir.exists());
        drop(cleaner);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn startup_cleanup_skips_active_owner() {
        let active = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        active.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let cleaner = cache(4096);
        cleaner.cleanup_stale_owners_with_limits(128, 128 * 1024 * 1024);
        assert!(active.dir.exists());
        assert!(matches!(
            active.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
    }

    #[test]
    fn startup_cleanup_truncates_stale_files_within_byte_budget() {
        let namespace = isolated_namespace();
        let cleaner =
            PreparedDiskCache::open(namespace.clone(), &epoch(), 4096, Work::default()).unwrap();
        let (dir, file_path) = stale_owner(&cleaner, 50);
        for remaining in [34, 18, 2] {
            cleaner.cleanup_stale_owners_with_limits(128, 16);
            assert_eq!(fs::metadata(&file_path).unwrap().len(), remaining);
            assert!(dir.exists());
        }
        cleaner.cleanup_stale_owners_with_limits(128, 16);
        assert!(!dir.exists());
        drop(cleaner);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn replaced_entry_symlink_cannot_escape_private_directory() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let path = cache.entry_path(entry.digest);
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(
            cache.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn lookup_counts_exactly_the_bytes_it_reads() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let mut missed = DiskRead::default();
        assert!(matches!(
            cache.lookup(&entry, &mut missed).unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!((missed.operations, missed.bytes), (0, 0));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let stored = cache.entry_file_len(&entry);
        let mut hit = DiskRead::default();
        assert!(matches!(
            cache.lookup(&entry, &mut hit).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert_eq!((hit.operations, hit.bytes), (1, stored));
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        let mut corrupt = DiskRead::default();
        assert!(matches!(
            cache.lookup(&entry, &mut corrupt).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!((corrupt.operations, corrupt.bytes), (1, stored));
        for truncated in [stored - 1, 0] {
            cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
            OpenOptions::new()
                .write(true)
                .open(cache.entry_path(entry.digest))
                .unwrap()
                .set_len(truncated)
                .unwrap();
            let mut short = DiskRead::default();
            assert!(matches!(
                cache.lookup(&entry, &mut short).unwrap(),
                DiskLookup::Retired
            ));
            assert_eq!(
                (short.operations, short.bytes),
                (0, 0),
                "an entry truncated to {truncated} bytes was read"
            );
        }
    }

    #[test]
    fn size_estimates_read_the_index_while_file_access_rejects_a_replaced_owner_directory() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let estimates = (cache.decoded_bytes(&entry), cache.read_bound(&entry));
        assert_eq!(estimates.1, Some(cache.entry_file_len(&entry) as usize + 1));
        let displaced = cache.dir.with_extension("displaced");
        fs::rename(&cache.dir, &displaced).unwrap();
        DirBuilder::new().mode(0o700).create(&cache.dir).unwrap();
        assert_eq!(
            (cache.decoded_bytes(&entry), cache.read_bound(&entry)),
            estimates
        );
        let mut read = DiskRead::default();
        let refused = cache
            .lookup(&entry, &mut read)
            .err()
            .map(|error| error.reason);
        let inserted = cache
            .insert(
                &key(stamp(2), &json!({"root":"/repo"})),
                &facts(),
                |_| true,
                |_| true,
            )
            .err()
            .map(|error| error.reason);
        let listed = cache.has_entry(&entry).err().map(|error| error.reason);
        fs::remove_dir(&cache.dir).unwrap();
        fs::rename(&displaced, &cache.dir).unwrap();
        let changed = Some("prepared facts cache directory changed".to_owned());
        assert_eq!(
            (refused, inserted, listed),
            (changed.clone(), changed.clone(), changed)
        );
        assert_eq!((read.operations, read.bytes), (0, 0));
    }

    #[test]
    fn growth_after_the_length_check_reads_no_further_than_the_read_bound() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let bound = cache.read_bound(&entry).unwrap();
        let path = cache.entry_path(entry.digest);
        cache.set_read_hook(Some(Box::new(move || {
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"grown")
                .unwrap()
        })));
        let mut read = DiskRead::default();
        let outcome = cache.lookup(&entry, &mut read);
        cache.set_read_hook(None);
        assert!(matches!(outcome.unwrap(), DiskLookup::Retired));
        assert_eq!((read.operations, read.bytes as usize), (1, bound));
        assert_eq!(cache.stats().entries, 0);
    }

    fn pushed_facts(events: usize, tools: usize) -> PreparedFacts {
        let mut override_events = Vec::new();
        for event in 0..events {
            let mut names = Vec::new();
            for tool in 0..tools {
                names.push(format!("Tool{tool}"));
            }
            override_events.push(OverrideEvent {
                text: format!("event {event}"),
                tools: names,
            });
        }
        let mut facts = PreparedFacts {
            inputs: json!({
                "calls": [["Read", ["/repo/file.rs"]], ["Edit", ["/repo/a.rs", "/repo/b.rs"]]],
                "commands": ["true"],
                "edited_files": [{"path": "/repo/a.rs"}, {"path": "/repo/b.rs"}],
                "skills": [],
            }),
            has_error: false,
            override_events: Some(override_events),
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
    }

    fn decode_transients_walk(payload: usize) -> usize {
        assert_eq!(size_of::<Value>(), 16);
        let node_buffer = size_of::<Vec<Value>>() + (payload / 2 + 2) * size_of::<Value>();
        let unescape_scratch = 2 * (payload + 32);
        node_buffer + unescape_scratch
    }

    #[test]
    fn decoded_bytes_cover_the_file_and_the_decoded_facts() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert_eq!(cache.decoded_bytes(&entry), None);
        let built = pushed_facts(5, 5);
        assert!(cache.insert(&entry, &built, |_| true, |_| true).unwrap());
        let file = fs::metadata(cache.entry_path(entry.digest)).unwrap().len() as usize;
        assert_eq!(
            cache.decoded_bytes(&entry),
            Some(file + built.accounted_bytes() + decode_transients_walk(file - 80))
        );
        let DiskLookup::Hit(decoded) = cache.lookup(&entry, &mut DiskRead::default()).unwrap()
        else {
            panic!("expected completed facts");
        };
        assert!(
            decoded.accounted_bytes() <= built.accounted_bytes(),
            "decoded facts charge {} above the {} recorded at insertion",
            decoded.accounted_bytes(),
            built.accounted_bytes()
        );
        let (decoded_events, built_events) = (
            decoded.override_events.as_ref().unwrap(),
            built.override_events.as_ref().unwrap(),
        );
        assert_eq!(decoded_events.capacity(), built_events.capacity());
        for (decoded_event, built_event) in decoded_events.iter().zip(built_events) {
            assert_eq!(decoded_event.text.capacity(), decoded_event.text.len());
            assert_eq!(decoded_event.tools.capacity(), built_event.tools.capacity());
        }
    }

    #[test]
    fn decoded_bytes_cover_a_heap_node_buffer_for_a_large_payload() {
        let cache = cache(2 * 1024 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let mut built = PreparedFacts {
            inputs: json!({
                "calls": [["Read", ["/repo/file.rs"]]],
                "commands": [],
                "edited_files": [],
                "skills": [],
            }),
            has_error: false,
            override_events: Some(vec![OverrideEvent {
                text: "override line\n".repeat(32 * 1024),
                tools: vec!["Read".to_owned()],
            }]),
            accounted: 0,
        };
        built.refresh_accounted();
        assert!(cache.insert(&entry, &built, |_| true, |_| true).unwrap());
        let file = fs::metadata(cache.entry_path(entry.digest)).unwrap().len() as usize;
        let payload = file - 80;
        assert!(payload >= 400 * 1024, "payload {payload}");
        assert!(
            (payload - r#"{"inputs":"#.len()) / 2 + 2 >= (3 << 20) / size_of::<Value>(),
            "payload {payload} decodes through the thread-local node buffer"
        );
        assert_eq!(
            cache.decoded_bytes(&entry),
            Some(file + built.accounted_bytes() + decode_transients_walk(payload))
        );
        let DiskLookup::Hit(decoded) = cache.lookup(&entry, &mut DiskRead::default()).unwrap()
        else {
            panic!("expected completed facts");
        };
        assert!(
            decoded.accounted_bytes() <= built.accounted_bytes(),
            "decoded facts charge {} above the {} recorded at insertion",
            decoded.accounted_bytes(),
            built.accounted_bytes()
        );
        let (decoded_event, built_event) = (
            &decoded.override_events.as_ref().unwrap()[0],
            &built.override_events.as_ref().unwrap()[0],
        );
        assert_eq!(decoded_event.text, built_event.text);
        assert_eq!(decoded_event.text.capacity(), decoded_event.text.len());
        assert_eq!(decoded_event.tools, built_event.tools);
        assert_eq!(decoded.inputs, built.inputs);
    }

    #[test]
    fn insert_admits_its_payload_before_allocating_it() {
        let cache = cache(64 * 1024);
        let sample = pushed_facts(3, 2);
        let payload = sonic_rs::to_vec(&sample).unwrap().len();
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let admissions = std::cell::RefCell::new(Vec::new());
        assert!(!cache
            .insert(
                &entry,
                &sample,
                |bytes| {
                    admissions.borrow_mut().push(("reserve", bytes));
                    false
                },
                |growth| {
                    admissions.borrow_mut().push(("admit", growth));
                    true
                },
            )
            .unwrap());
        assert_eq!(admissions.replace(Vec::new()), [("reserve", payload)]);
        assert!(!cache.has_entry(&entry).unwrap());
        assert!(!cache.entry_path(entry.digest).exists());
        assert!(only_lock_remains(cache.dir_file.as_raw_fd()));
        assert_eq!(cache.index_capacity_bytes(), 0);
        assert_eq!(cache.audit_index_bytes(), 0);
        let stats = cache.stats();
        assert_eq!(
            (stats.entries, stats.bytes, stats.writes, stats.write_bytes),
            (0, 0, 0, 0)
        );
        assert!(cache
            .insert(
                &entry,
                &sample,
                |bytes| {
                    admissions.borrow_mut().push(("reserve", bytes));
                    true
                },
                |growth| {
                    admissions.borrow_mut().push(("admit", growth));
                    true
                },
            )
            .unwrap());
        let recorded = admissions.replace(Vec::new());
        assert_eq!(recorded[0], ("reserve", payload));
        assert_eq!((recorded.len(), recorded[1].0), (2, "admit"));
        assert!(recorded[1].1 > 0);
        assert_eq!(
            cache.entry_file_len(&entry),
            (HEADER_BYTES + payload) as u64
        );
        let stats = cache.stats();
        assert_eq!(
            (stats.entries, stats.bytes, stats.writes, stats.write_bytes),
            (
                1,
                HEADER_BYTES + payload,
                1,
                (HEADER_BYTES + payload) as u64
            )
        );
        let DiskLookup::Hit(decoded) = cache.lookup(&entry, &mut DiskRead::default()).unwrap()
        else {
            panic!("expected completed facts");
        };
        assert_eq!(decoded.inputs, sample.inputs);
    }

    #[test]
    fn insert_admits_its_index_growth_before_the_index_grows() {
        let cache = cache(64 * 1024);
        let sample = pushed_facts(1, 1);
        let first = key(stamp(1), &json!({"root":"/repo"}));
        let offered = std::cell::Cell::new(0);
        assert!(!cache
            .insert(
                &first,
                &sample,
                |_| true,
                |growth| {
                    offered.set(growth);
                    false
                }
            )
            .unwrap());
        assert!(offered.get() > 0);
        assert!(!cache.has_entry(&first).unwrap());
        assert!(!cache.entry_path(first.digest).exists());
        assert_eq!(cache.index_capacity_bytes(), 0);
        assert_eq!(cache.audit_index_bytes(), 0);
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.bytes, stats.writes), (0, 0, 0));
        assert!(cache.insert(&first, &sample, |_| true, |_| true).unwrap());
        assert_eq!(cache.index_capacity_bytes(), offered.get());
        assert_eq!(cache.audit_index_bytes(), offered.get());
        assert!(matches!(
            cache.lookup(&first, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        let second = key(stamp(2), &json!({"root":"/repo"}));
        assert!(cache
            .insert(
                &second,
                &sample,
                |_| true,
                |growth| {
                    offered.set(growth);
                    true
                }
            )
            .unwrap());
        assert_eq!(offered.get(), 0);
        assert_eq!(cache.index_capacity_bytes(), cache.audit_index_bytes());
        assert_eq!(cache.stats().entries, 2);
    }
}

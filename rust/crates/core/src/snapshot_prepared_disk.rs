#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use sha2::{Digest, Sha256};
use sonic_rs::{json, JsonValueTrait, Value};

use crate::snapshot::{SnapshotError, SourceStamp, Status, PARSER_VERSION};
#[cfg(test)]
use crate::snapshot_ledger::Reserved;
use crate::snapshot_ledger::{Table, Work};
use crate::snapshot_memory::sonic_node_buffer_bytes;
use crate::snapshot_prepared::PreparedFacts;

const VERSION: &[u8; 8] = b"CTPF0002";
pub(crate) const HEADER_BYTES: usize = 88;
const SONIC_STRING_BLOCK_LANES: usize = 32;
const NAMESPACE: &str = "cc-transcript-prepared";
const REUSE_NAMESPACE: &str = "cc-transcript-prepared-reuse";
const ENGINE_BINDING: &str = "engine";
const OWNER_LOCK: &CStr = c"owner.lock";
const STAGING: &CStr = c"staging";
const MAX_SCANNED_OWNERS: usize = 32;
const MAX_CLEANED_OWNERS: usize = 8;
const MAX_CLEANED_FILES: usize = 128;
const MAX_SCANNED_FILES_PER_OWNER: usize = 256;
const MAX_CLEANED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CLAIMED_OWNERS: usize = 8;
const MAX_LISTED_OWNERS: usize = 4096;
const MAX_INHERITED_GENERATIONS: u64 = 3;
const MAX_LISTED_NAMES: usize = 16384;
const MAX_INHERITED_ENTRIES: usize = 8192;
const INSERT_SWEEP_NAMES: usize = 16;
const ROOM_SWEEP_NAMES: usize = 256;
const ENTRY_NAME_BYTES: usize = 118;

static ACTIVE_OWNERS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[cfg(test)]
thread_local! {
    pub(crate) static REUSE_NAMESPACE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

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

fn namespace_path(name: &str) -> io::Result<PathBuf> {
    Ok(fs::canonicalize(std::env::temp_dir())?.join(name))
}

fn reuse_namespace() -> io::Result<PathBuf> {
    #[cfg(test)]
    {
        if let Some(namespace) =
            REUSE_NAMESPACE_OVERRIDE.with(|namespace| namespace.borrow().clone())
        {
            return Ok(namespace);
        }
    }
    namespace_path(REUSE_NAMESPACE)
}

fn seal(accounted: u64, payload: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(accounted.to_le_bytes())
        .chain_update(payload)
        .finalize()
        .into()
}

fn tag(compatibility: &[u8; 32]) -> u64 {
    u64::from_le_bytes(compatibility[..8].try_into().expect("compatibility tag"))
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
        || EntryName::parse(name).is_some()
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

#[cfg(target_os = "linux")]
fn clear_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(not(target_os = "linux"))]
fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}

struct DirEntries {
    dir: *mut libc::DIR,
    failure: Option<io::ErrorKind>,
}

impl DirEntries {
    fn new(dir_fd: libc::c_int) -> io::Result<Self> {
        let file = openat_file(dir_fd, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
        let dir = unsafe { libc::fdopendir(file.as_raw_fd()) };
        if dir.is_null() {
            return Err(io::Error::last_os_error());
        }
        std::mem::forget(file);
        Ok(Self { dir, failure: None })
    }

    fn next(&mut self) -> Option<CString> {
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(self.dir) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    self.failure = Some(error.kind());
                }
                return None;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            return Some(name.to_owned());
        }
    }

    fn failure(&self) -> Option<io::ErrorKind> {
        self.failure
    }
}

impl Drop for DirEntries {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.dir) };
    }
}

unsafe impl Send for DirEntries {}

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

pub struct FactOrigin<'a> {
    pub producer: Option<&'a str>,
    pub max_source_bytes: usize,
    pub max_entry_bytes: usize,
}

impl FactOrigin<'_> {
    fn compatibility(&self, binding: &str) -> Result<[u8; 32], SnapshotError> {
        let identity = json!({
            "version": "prepared-facts/2",
            "parser": PARSER_VERSION,
            "producer": self.producer,
            "max_source_bytes": self.max_source_bytes.to_string(),
            "max_entry_bytes": self.max_entry_bytes.to_string(),
            "binding": binding,
        });
        let canonical = crate::ids::canonical_json(&identity)
            .map_err(|error| incomplete(format!("prepared facts compatibility: {error}")))?;
        Ok(Sha256::digest(canonical.as_bytes()).into())
    }
}

struct EntryHeader {
    binding: [u8; 32],
    payload: u64,
    accounted: u64,
    seal: [u8; 32],
}

impl EntryHeader {
    fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..8].copy_from_slice(VERSION);
        bytes[8..40].copy_from_slice(&self.binding);
        bytes[40..48].copy_from_slice(&self.payload.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.accounted.to_le_bytes());
        bytes[56..].copy_from_slice(&self.seal);
        bytes
    }

    fn decode(bytes: &[u8; HEADER_BYTES]) -> Option<Self> {
        (bytes[..8] == *VERSION).then(|| Self {
            binding: bytes[8..40].try_into().expect("binding header"),
            payload: u64::from_le_bytes(bytes[40..48].try_into().expect("length header")),
            accounted: u64::from_le_bytes(bytes[48..56].try_into().expect("accounted header")),
            seal: bytes[56..].try_into().expect("seal header"),
        })
    }
}

struct EntryName {
    key: [u8; 32],
    tag: u64,
    bytes: usize,
    accounted: usize,
}

impl EntryName {
    fn encode(&self) -> CString {
        let high = u128::from_be_bytes(self.key[..16].try_into().expect("entry name key"));
        let low = u128::from_be_bytes(self.key[16..].try_into().expect("entry name key"));
        CString::new(format!(
            "{:032x}{:032x}{:016x}{:016x}{:016x}.facts",
            high, low, self.tag, self.bytes, self.accounted
        ))
        .expect("entry file name")
    }

    fn legacy(key: &[u8; 32]) -> CString {
        let high = u128::from_be_bytes(key[..16].try_into().expect("entry name key"));
        let low = u128::from_be_bytes(key[16..].try_into().expect("entry name key"));
        CString::new(format!("{high:032x}{low:032x}.facts")).expect("entry file name")
    }

    fn parse(name: &CStr) -> Option<Self> {
        let bytes = name.to_bytes();
        if bytes.len() != ENTRY_NAME_BYTES
            || !bytes.ends_with(b".facts")
            || !hex_bytes(&bytes[..112])
        {
            return None;
        }
        let text = std::str::from_utf8(&bytes[..112]).ok()?;
        let high = u128::from_str_radix(&text[..32], 16).ok()?;
        let low = u128::from_str_radix(&text[32..64], 16).ok()?;
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&high.to_be_bytes());
        key[16..].copy_from_slice(&low.to_be_bytes());
        Some(Self {
            key,
            tag: u64::from_str_radix(&text[64..80], 16).ok()?,
            bytes: usize::from_str_radix(&text[80..96], 16).ok()?,
            accounted: usize::from_str_radix(&text[96..112], 16).ok()?,
        })
    }
}

pub struct PreparedDiskKey {
    digest: [u8; 32],
    name: [u8; 32],
    native: bool,
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
        let digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
        Ok(Self {
            digest,
            name: Sha256::digest(digest).into(),
            native: classifier["id"].as_str() == Some("native")
                && classifier["version"].as_str() == Some("1"),
        })
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

struct ClaimedOwner {
    generation: u64,
    name: CString,
    dir: std::fs::File,
    _lock: std::fs::File,
    cursor: Option<DirEntries>,
    swept: bool,
    garbage: usize,
    stuck: usize,
    dirty: bool,
    listed: bool,
}

enum Pass {
    Paused,
    Unreadable,
    Dirty,
    Clean,
}

impl ClaimedOwner {
    fn gone(&mut self) -> Pass {
        self.stuck = 0;
        self.dirty = false;
        Pass::Clean
    }
}

struct DiskState {
    entries: Table<[u8; 32], DiskEntry>,
    retired: usize,
    bytes: usize,
    inherited_bytes: usize,
    claimed: Vec<ClaimedOwner>,
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
            inherited_bytes: 0,
            claimed: Vec::new(),
            clock: MAX_CLAIMED_OWNERS as u64,
            writes: 0,
            write_bytes: 0,
        }
    }

    fn garbage_bytes(&self) -> usize {
        self.claimed
            .iter()
            .fold(0, |sum, owner| sum.saturating_add(owner.garbage))
    }

    fn unread(&self) -> bool {
        self.claimed.iter().any(|owner| !owner.listed)
    }
}

pub struct PreparedDiskCache {
    namespace: PathBuf,
    namespace_file: std::fs::File,
    dir: PathBuf,
    dir_file: std::fs::File,
    owner_name: CString,
    owner_lock: std::fs::File,
    dir_device: u64,
    dir_inode: u64,
    max_bytes: usize,
    owner_compatibility: [u8; 32],
    engine_compatibility: Option<[u8; 32]>,
    state: Mutex<DiskState>,
}

impl PreparedDiskCache {
    pub fn new(
        owner_epoch: &str,
        max_bytes: usize,
        origin: &FactOrigin<'_>,
        headroom: usize,
        work: Work,
    ) -> Result<Self, SnapshotError> {
        let namespace = if origin.producer.is_some() {
            reuse_namespace()
        } else {
            namespace_path(NAMESPACE)
        };
        Self::open(
            namespace.map_err(disk_error)?,
            owner_epoch,
            max_bytes,
            origin,
            headroom,
            work,
        )
    }

    fn open(
        namespace: PathBuf,
        owner_epoch: &str,
        max_bytes: usize,
        origin: &FactOrigin<'_>,
        headroom: usize,
        work: Work,
    ) -> Result<Self, SnapshotError> {
        if owner_epoch.len() != 64 || !owner_epoch.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(incomplete("invalid prepared facts owner epoch"));
        }
        if max_bytes <= HEADER_BYTES {
            return Err(incomplete("prepared facts disk cache capacity exhausted"));
        }
        let owner_compatibility = origin.compatibility(owner_epoch)?;
        let engine_compatibility = match origin.producer {
            Some(_) => Some(origin.compatibility(ENGINE_BINDING)?),
            None => None,
        };
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
            owner_lock,
            dir_device: metadata.dev(),
            dir_inode: metadata.ino(),
            max_bytes,
            owner_compatibility,
            engine_compatibility,
            state: Mutex::new(DiskState::new(work)),
        };
        match cache.engine_compatibility {
            None => cache.cleanup_stale_owners(),
            Some(engine) => cache.inherit(tag(&engine), headroom),
        }
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
            if !self.foreign_owner(&name) {
                continue;
            }
            self.cleanup_stale_owner(&name, &mut budget);
        }
    }

    fn foreign_owner(&self, name: &CStr) -> bool {
        let bytes = name.to_bytes();
        bytes.len() == 64
            && hex_bytes(bytes)
            && name != self.owner_name.as_c_str()
            && !ACTIVE_OWNERS
                .lock()
                .expect("active prepared owners")
                .contains(std::str::from_utf8(bytes).expect("hex owner epoch"))
    }

    fn lock_stale_owner(&self, name: &CStr) -> Option<(std::fs::File, std::fs::File)> {
        let owner = openat_file(
            self.namespace_file.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .ok()?;
        if !private_dir(&owner.metadata().ok()?) {
            return None;
        }
        let lock = openat_file(owner.as_raw_fd(), OWNER_LOCK, libc::O_RDWR).ok()?;
        if !private_file(&lock)
            || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        {
            return None;
        }
        Some((owner, lock))
    }

    fn remove_emptied_owner(&self, name: &CStr, dir: &std::fs::File) -> bool {
        if !only_lock_remains(dir.as_raw_fd()) {
            return false;
        }
        if unlinkat_file(dir.as_raw_fd(), OWNER_LOCK, 0).is_ok()
            && unlinkat_file(self.namespace_file.as_raw_fd(), name, libc::AT_REMOVEDIR).is_err()
        {
            restore_lock(dir.as_raw_fd());
        }
        true
    }

    fn cleanup_stale_owner(&self, name: &CStr, budget: &mut CleanupBudget) {
        let Some((owner, _lock)) = self.lock_stale_owner(name) else {
            return;
        };
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
        if complete {
            self.remove_emptied_owner(name, &owner);
        }
    }

    fn inherit(&self, engine_tag: u64, headroom: usize) {
        let mut claimed: Vec<ClaimedOwner> = Vec::new();
        let mut candidates: Vec<CString> = Vec::new();
        if let Ok(mut owners) = DirEntries::new(self.namespace_file.as_raw_fd()) {
            for _ in 0..MAX_LISTED_OWNERS {
                let Some(name) = owners.next() else {
                    break;
                };
                if self.foreign_owner(&name) {
                    candidates.push(name);
                }
            }
        }
        let start = tag(&self.owner_compatibility) as usize % candidates.len().max(1);
        candidates.rotate_left(start);
        for name in candidates.into_iter().take(MAX_SCANNED_OWNERS) {
            if claimed.len() == MAX_CLAIMED_OWNERS {
                break;
            }
            let Some((dir, lock)) = self.lock_stale_owner(&name) else {
                continue;
            };
            if self.remove_emptied_owner(&name, &dir) {
                continue;
            }
            let mut stored = [0u8; 8];
            let generation = match lock.read_at(&mut stored, 0) {
                Ok(8) => u64::from_le_bytes(stored),
                _ => 0,
            };
            claimed.push(ClaimedOwner {
                generation,
                name,
                dir,
                _lock: lock,
                cursor: None,
                swept: false,
                garbage: 0,
                stuck: 0,
                dirty: false,
                listed: false,
            });
        }
        claimed.sort_by(|left, right| {
            left.generation
                .cmp(&right.generation)
                .then_with(|| left.name.cmp(&right.name))
        });
        let newest = claimed.last().map_or(0, |owner| owner.generation);
        let _ = self
            .owner_lock
            .write_all_at(&newest.saturating_add(1).to_le_bytes(), 0);
        let mut state = self.state.lock().expect("prepared facts disk state");
        let mut examined = 0;
        let mut growth = 0usize;
        let mut largest = 0usize;
        'listing: for rank in (0..claimed.len()).rev() {
            let Ok(mut names) = DirEntries::new(claimed[rank].dir.as_raw_fd()) else {
                continue;
            };
            while let Some(name) = names.next() {
                if examined == MAX_LISTED_NAMES {
                    break 'listing;
                }
                examined += 1;
                let Some(parsed) = EntryName::parse(&name) else {
                    continue;
                };
                let index_growth = state.entries.growth_for(&parsed.key);
                let decoded = (HEADER_BYTES..=self.max_bytes)
                    .contains(&parsed.bytes)
                    .then(|| decode_transient_bytes(parsed.bytes - HEADER_BYTES))
                    .and_then(|transient| parsed.accounted.checked_add(parsed.bytes + transient))
                    .filter(|decoded| {
                        parsed.tag == engine_tag
                            && newest - claimed[rank].generation < MAX_INHERITED_GENERATIONS
                            && !state.entries.contains_key(&parsed.key)
                            && parsed.bytes <= self.max_bytes - state.inherited_bytes
                            && state.entries.len() < MAX_INHERITED_ENTRIES
                            && growth
                                .saturating_add(index_growth)
                                .saturating_add(largest.max(*decoded))
                                <= headroom
                    });
                match decoded {
                    Some(decoded) => {
                        growth += index_growth;
                        largest = largest.max(decoded);
                        state.entries.reserve_for(&parsed.key);
                        state.entries.insert(
                            parsed.key,
                            DiskEntry {
                                bytes: parsed.bytes,
                                accounted: parsed.accounted,
                                last_used: rank as u64,
                            },
                        );
                        state.inherited_bytes += parsed.bytes;
                    }
                    None => {
                        claimed[rank].garbage = claimed[rank]
                            .garbage
                            .saturating_add(parsed.bytes.min(self.max_bytes));
                    }
                }
            }
            claimed[rank].listed = names.failure().is_none();
        }
        state.claimed = claimed;
    }

    fn compatibility(&self, key: &PreparedDiskKey) -> [u8; 32] {
        match self.engine_compatibility {
            Some(engine) if key.native => engine,
            _ => self.owner_compatibility,
        }
    }

    fn binding(&self, key: &PreparedDiskKey) -> [u8; 32] {
        Sha256::new()
            .chain_update(key.digest)
            .chain_update(self.compatibility(key))
            .finalize()
            .into()
    }

    fn entry_name(&self, key: &PreparedDiskKey, entry: &DiskEntry) -> CString {
        match self.engine_compatibility {
            None => EntryName::legacy(&key.name),
            Some(_) => EntryName {
                key: key.name,
                tag: tag(&self.compatibility(key)),
                bytes: entry.bytes,
                accounted: entry.accounted,
            }
            .encode(),
        }
    }

    fn entry_names(&self, name: &[u8; 32], entry: &DiskEntry) -> Vec<CString> {
        match self.engine_compatibility {
            None => vec![EntryName::legacy(name)],
            Some(engine) => [self.owner_compatibility, engine]
                .iter()
                .map(|compatibility| {
                    EntryName {
                        key: *name,
                        tag: tag(compatibility),
                        bytes: entry.bytes,
                        accounted: entry.accounted,
                    }
                    .encode()
                })
                .collect(),
        }
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
        let name: [u8; 32] = Sha256::digest(digest).into();
        let state = self.state.lock().expect("prepared facts disk state");
        if let Some(entry) = state.entries.get(&name) {
            let candidates: Vec<PathBuf> = self
                .entry_names(&name, entry)
                .iter()
                .map(|candidate| self.dir.join(candidate.to_str().expect("entry file name")))
                .collect();
            return candidates
                .iter()
                .find(|candidate| candidate.exists())
                .unwrap_or(&candidates[0])
                .clone();
        }
        let bare = EntryName {
            key: name,
            tag: 0,
            bytes: 0,
            accounted: 0,
        }
        .encode();
        let prefix = &bare.to_str().expect("entry file name")[..64];
        fs::read_dir(&self.dir)
            .expect("prepared facts cache directory")
            .filter_map(Result::ok)
            .map(|listed| listed.path())
            .find(|path| {
                path.file_name()
                    .and_then(|file| file.to_str())
                    .is_some_and(|file| file.starts_with(prefix))
            })
            .unwrap_or_else(|| self.dir.join(format!("{prefix}.facts")))
    }

    fn retire(&self, state: &mut DiskState, name: [u8; 32]) -> Result<(), SnapshotError> {
        let Some(entry) = state.entries.get(&name) else {
            return Ok(());
        };
        let bytes = entry.bytes;
        let rank = entry.last_used as usize;
        let inherited = entry.last_used < MAX_CLAIMED_OWNERS as u64;
        let dir_fd = if inherited {
            state.claimed[rank].dir.as_raw_fd()
        } else {
            self.dir_file.as_raw_fd()
        };
        let mut stranded = false;
        for candidate in self.entry_names(&name, entry) {
            match unlinkat_file(dir_fd, &candidate, 0) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) if inherited => stranded = true,
                Err(error) => return Err(disk_error(error)),
            }
        }
        if inherited {
            state.inherited_bytes -= bytes;
            let owner = &mut state.claimed[rank];
            if stranded {
                owner.garbage = owner.garbage.saturating_add(bytes);
                owner.stuck = 0;
                owner.dirty = false;
                owner.cursor = None;
                owner.swept = false;
            } else if owner.cursor.is_some() {
                owner.dirty = true;
            }
        } else {
            state.bytes -= bytes;
        }
        state.entries.remove(&name);
        state.retired += 1;
        Ok(())
    }

    fn over_budget(&self, state: &DiskState, size: usize) -> bool {
        state
            .bytes
            .saturating_add(state.inherited_bytes)
            .saturating_add(state.garbage_bytes())
            .saturating_add(size)
            > self.max_bytes
    }

    fn make_room(&self, state: &mut DiskState, size: usize) -> Result<bool, SnapshotError> {
        if state.unread() || (state.garbage_bytes() > 0 && self.over_budget(state, size)) {
            self.sweep(state, ROOM_SWEEP_NAMES);
        }
        let unread = state.unread();
        let size = if unread { 0 } else { size };
        while self.over_budget(state, size) {
            if state.garbage_bytes().saturating_add(size) > self.max_bytes {
                return Ok(false);
            }
            let oldest = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(name, _)| *name);
            match oldest {
                Some(name) => self.retire(state, name)?,
                None => return Err(incomplete("prepared facts disk cache capacity exhausted")),
            }
        }
        Ok(!unread)
    }

    fn sweep_pass(&self, state: &mut DiskState, rank: usize, remaining: &mut usize) -> Pass {
        let dir_fd = state.claimed[rank].dir.as_raw_fd();
        let opened = match state.claimed[rank].cursor.take() {
            Some(cursor) => Ok(cursor),
            None => DirEntries::new(dir_fd),
        };
        let mut cursor = match opened {
            Ok(cursor) => cursor,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return state.claimed[rank].gone();
            }
            Err(_) => return Pass::Unreadable,
        };
        while *remaining > 0 {
            let Some(name) = cursor.next() else {
                let owner = &mut state.claimed[rank];
                return match cursor.failure() {
                    None => {
                        if std::mem::take(&mut owner.dirty) {
                            Pass::Dirty
                        } else {
                            Pass::Clean
                        }
                    }
                    Some(io::ErrorKind::NotFound) => owner.gone(),
                    Some(_) => {
                        owner.stuck = 0;
                        Pass::Unreadable
                    }
                };
            };
            if name.as_c_str() == OWNER_LOCK {
                continue;
            }
            *remaining -= 1;
            let size = match EntryName::parse(&name) {
                Some(parsed) => {
                    let kept = self
                        .engine_compatibility
                        .is_some_and(|engine| parsed.tag == tag(&engine))
                        && state.entries.get(&parsed.key).is_some_and(|entry| {
                            entry.last_used == rank as u64
                                && entry.bytes == parsed.bytes
                                && entry.accounted == parsed.accounted
                        });
                    if kept {
                        continue;
                    }
                    parsed.bytes.min(self.max_bytes)
                }
                None if generated_entry(&name) => 0,
                None => continue,
            };
            let owner = &mut state.claimed[rank];
            match unlinkat_file(dir_fd, &name, 0) {
                Ok(()) => {
                    owner.garbage = owner.garbage.saturating_sub(size);
                    owner.dirty = true;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => owner.stuck = owner.stuck.saturating_add(size),
            }
        }
        state.claimed[rank].cursor = Some(cursor);
        Pass::Paused
    }

    fn sweep(&self, state: &mut DiskState, names: usize) {
        let mut remaining = names;
        for rank in 0..state.claimed.len() {
            while !state.claimed[rank].swept {
                match self.sweep_pass(state, rank, &mut remaining) {
                    Pass::Paused => return,
                    Pass::Unreadable => break,
                    Pass::Dirty => state.claimed[rank].stuck = 0,
                    Pass::Clean => {
                        let owner = &mut state.claimed[rank];
                        owner.listed = true;
                        owner.garbage = std::mem::take(&mut owner.stuck);
                        owner.swept = true;
                        if owner.garbage == 0 {
                            self.remove_emptied_owner(
                                &state.claimed[rank].name,
                                &state.claimed[rank].dir,
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn lookup(
        &self,
        key: &PreparedDiskKey,
        read: &mut DiskRead,
    ) -> Result<DiskLookup, SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        let Some(entry) = state.entries.get(&key.name) else {
            return Ok(DiskLookup::Miss);
        };
        let (expected, accounted, last_used) = (entry.bytes, entry.accounted, entry.last_used);
        let name = self.entry_name(key, entry);
        let inherited = last_used < MAX_CLAIMED_OWNERS as u64;
        let dir_fd = if inherited {
            state.claimed[last_used as usize].dir.as_raw_fd()
        } else {
            self.dir_file.as_raw_fd()
        };
        let result = (|| -> Result<PreparedFacts, SnapshotError> {
            let file = openat_file(dir_fd, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                .map_err(disk_error)?;
            let metadata = file.metadata().map_err(disk_error)?;
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
            let verified = bytes
                .get(..HEADER_BYTES)
                .and_then(|header| <&[u8; HEADER_BYTES]>::try_from(header).ok())
                .and_then(EntryHeader::decode)
                .is_some_and(|header| {
                    bytes.len() == expected
                        && header.binding == self.binding(key)
                        && header.payload == (expected - HEADER_BYTES) as u64
                        && header.accounted == accounted as u64
                        && header.seal == seal(header.accounted, &bytes[HEADER_BYTES..])
                });
            if !verified {
                return Err(incomplete("prepared facts cache integrity check failed"));
            }
            let mut facts: PreparedFacts = sonic_rs::from_slice(&bytes[HEADER_BYTES..])
                .map_err(|_| incomplete("prepared facts cache payload invalid"))?;
            facts.refresh_accounted();
            if inherited {
                if facts.accounted_bytes() > accounted {
                    return Err(incomplete("prepared facts cache accounting exceeded"));
                }
                renameat_file(dir_fd, &name, self.dir_file.as_raw_fd(), &name)
                    .map_err(disk_error)?;
            }
            Ok(facts)
        })();
        match result {
            Ok(facts) => {
                state.clock += 1;
                let clock = state.clock;
                state
                    .entries
                    .get_mut(&key.name)
                    .expect("cached entry")
                    .last_used = clock;
                if inherited {
                    state.inherited_bytes -= expected;
                    state.bytes += expected;
                    let owner = &mut state.claimed[last_used as usize];
                    if owner.cursor.is_some() {
                        owner.dirty = true;
                    }
                    let _ = self.make_room(&mut state, 0);
                }
                self.sweep(&mut state, INSERT_SWEEP_NAMES);
                Ok(DiskLookup::Hit(facts))
            }
            Err(_) => {
                self.retire(&mut state, key.name)?;
                Ok(if inherited {
                    DiskLookup::Miss
                } else {
                    DiskLookup::Retired
                })
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
            .get(&key.name)
            .is_some_and(|entry| entry.last_used >= MAX_CLAIMED_OWNERS as u64))
    }

    pub fn decoded_bytes(&self, key: &PreparedDiskKey) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .map(|entry| {
                entry.bytes + entry.accounted + decode_transient_bytes(entry.bytes - HEADER_BYTES)
            })
    }

    pub fn read_bound(&self, key: &PreparedDiskKey) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .map(|entry| probed_read_bytes(entry.bytes))
    }

    pub(crate) fn index_bytes(&self) -> usize {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .reserved_bytes()
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
        let listed = state.entries.get(&key.name).map(|entry| entry.last_used);
        match listed {
            Some(last_used) if last_used >= MAX_CLAIMED_OWNERS as u64 => return Ok(true),
            Some(_) => self.retire(&mut state, key.name)?,
            None => {}
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
        if !self.make_room(&mut state, size)? {
            return Ok(false);
        }
        state.clock += 1;
        let temp = CString::new(format!("{:x}.tmp", state.clock)).expect("temp file name");
        let accounted = facts.accounted_bytes();
        let entry = DiskEntry {
            bytes: size,
            accounted,
            last_used: state.clock,
        };
        let name = self.entry_name(key, &entry);
        let header = EntryHeader {
            binding: self.binding(key),
            payload: payload.len() as u64,
            accounted: accounted as u64,
            seal: seal(accounted as u64, &payload),
        }
        .encode();
        let result = (|| -> Result<(), SnapshotError> {
            let mut file = self
                .open_entry(&temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
                .map_err(disk_error)?;
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(disk_error(io::Error::last_os_error()));
            }
            file.write_all(&header).map_err(disk_error)?;
            file.write_all(&payload).map_err(disk_error)?;
            drop(file);
            self.rename_entry(&temp, &name).map_err(disk_error)
        })();
        if let Err(error) = result {
            let _ = self.unlink_entry(&temp);
            return Err(error);
        }
        if !admit(state.entries.growth_for(&key.name)) {
            let _ = self.unlink_entry(&name);
            return Ok(false);
        }
        state.bytes += size;
        state.writes += 1;
        state.write_bytes += size as u64;
        state.entries.reserve_for(&key.name);
        state.entries.insert(key.name, entry);
        self.sweep(&mut state, INSERT_SWEEP_NAMES);
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

    fn discard_entries(&self, state: &DiskState) {
        let mut files = 0;
        let mut bytes = 0;
        for (name, entry) in state.entries.iter() {
            for candidate in self.entry_names(name, entry) {
                if files == MAX_CLEANED_FILES || bytes == MAX_CLEANED_BYTES {
                    return;
                }
                let Ok(file) = openat_file(self.dir_file.as_raw_fd(), &candidate, libc::O_RDWR)
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
                    return;
                }
                if unlinkat_file(self.dir_file.as_raw_fd(), &candidate, 0).is_ok() {
                    files += 1;
                    bytes += metadata.len();
                }
            }
        }
    }
}

impl Drop for PreparedDiskCache {
    fn drop(&mut self) {
        if self.check_dir().is_ok() {
            if self.engine_compatibility.is_none() {
                if let Ok(state) = self.state.lock() {
                    self.discard_entries(&state);
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
    use std::os::unix::fs::PermissionsExt;
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

    fn origin(producer: Option<&str>) -> FactOrigin<'_> {
        FactOrigin {
            producer,
            max_source_bytes: 1 << 20,
            max_entry_bytes: 1 << 20,
        }
    }

    fn cache(cap: usize) -> PreparedDiskCache {
        PreparedDiskCache::new(&epoch(), cap, &origin(None), 0, Work::default()).unwrap()
    }

    fn reuse_cache(namespace: &Path, cap: usize) -> PreparedDiskCache {
        PreparedDiskCache::open(
            namespace.to_path_buf(),
            &epoch(),
            cap,
            &origin(Some("producer-a")),
            1 << 20,
            Work::default(),
        )
        .unwrap()
    }

    fn engine_tag(origin: &FactOrigin<'_>) -> u64 {
        tag(&origin.compatibility(ENGINE_BINDING).unwrap())
    }

    fn left_behind(namespace: &Path, entry: &PreparedDiskKey, built: &PreparedFacts) -> PathBuf {
        let writer = reuse_cache(namespace, 64 * 1024);
        assert!(writer.insert(entry, built, |_| true, |_| true).unwrap());
        writer.entry_file(entry)
    }

    fn dead_owner(namespace: &Path) -> PathBuf {
        let id = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        let dir = namespace.join(format!(
            "{:032x}{:032x}",
            u64::MAX - std::process::id() as u64,
            u64::MAX - id
        ));
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("owner.lock"))
            .unwrap();
        dir
    }

    fn planted(dir: &Path, name: &CStr, bytes: usize) -> PathBuf {
        let path = dir.join(name.to_str().unwrap());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(&vec![1; bytes])
            .unwrap();
        path
    }

    fn revision(size: u64) -> PreparedDiskKey {
        key(stamp(size), &json!({"root":"/repo"}))
    }

    fn budget(cache: &PreparedDiskCache) -> (usize, usize, usize, usize) {
        let state = cache.state.lock().unwrap();
        (
            state.entries.len(),
            state.bytes,
            state.inherited_bytes,
            state.garbage_bytes(),
        )
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
        let namespace = namespace_path(NAMESPACE).unwrap();
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
        let cache =
            PreparedDiskCache::new(&epoch, 4096, &origin(None), 0, Work::default()).unwrap();
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
        let result = PreparedDiskCache::open(
            namespace.clone(),
            &epoch,
            4096,
            &origin(None),
            0,
            Work::default(),
        );
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
        let Err(error) = PreparedDiskCache::new(&epoch, 4096, &origin(None), 0, Work::default())
        else {
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
        let cleaner = PreparedDiskCache::open(
            namespace.clone(),
            &epoch(),
            4096,
            &origin(None),
            0,
            Work::default(),
        )
        .unwrap();
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
        let cleaner = PreparedDiskCache::open(
            namespace.clone(),
            &epoch(),
            4096,
            &origin(None),
            0,
            Work::default(),
        )
        .unwrap();
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
            Some(file + built.accounted_bytes() + decode_transients_walk(file - HEADER_BYTES))
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
        let payload = file - HEADER_BYTES;
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

    #[test]
    fn successor_lists_then_moves_and_verifies_an_unchanged_fact() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let first = reuse_cache(&namespace, 64 * 1024);
        assert!(first.insert(&entry, &built, |_| true, |_| true).unwrap());
        let estimates = (first.decoded_bytes(&entry), first.read_bound(&entry));
        let stored = first.entry_file_len(&entry);
        let left = first.entry_file(&entry);
        drop(first);
        assert_eq!(estimates.1, Some(stored as usize + 1));
        assert_eq!(fs::metadata(&left).unwrap().len(), stored);
        let second = reuse_cache(&namespace, 64 * 1024);
        let stats = second.stats();
        assert_eq!((stats.entries, stats.bytes, stats.writes), (1, 0, 0));
        assert_eq!(budget(&second), (1, 0, stored as usize, 0));
        assert!(!second.has_entry(&entry).unwrap());
        assert_eq!(
            (second.decoded_bytes(&entry), second.read_bound(&entry)),
            estimates
        );
        assert!(left.exists());
        let mut read = DiskRead::default();
        let DiskLookup::Hit(loaded) = second.lookup(&entry, &mut read).unwrap() else {
            panic!("expected inherited facts");
        };
        assert_eq!(loaded.inputs, built.inputs);
        assert_eq!((read.operations, read.bytes), (1, stored));
        assert!(second.has_entry(&entry).unwrap());
        assert_eq!(second.stats().bytes, stored as usize);
        assert_eq!(budget(&second), (1, stored as usize, 0, 0));
        let moved = second.entry_file(&entry);
        assert_eq!(moved.parent(), Some(second.dir.as_path()));
        assert_eq!(moved.file_name(), left.file_name());
        assert_eq!(fs::metadata(&moved).unwrap().len(), stored);
        assert!(!left.exists());
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn successor_rejects_facts_it_cannot_trust() {
        let built = pushed_facts(5, 5);
        let authority = json!({"root":"/repo"});
        for (case, classifier, reader) in [
            (
                "a different producer",
                json!({"id":"native","version":"1"}),
                origin(Some("producer-b")),
            ),
            (
                "a different max_source_bytes",
                json!({"id":"native","version":"1"}),
                FactOrigin {
                    producer: Some("producer-a"),
                    max_source_bytes: (1 << 20) + 1,
                    max_entry_bytes: 1 << 20,
                },
            ),
            (
                "a custom classifier",
                json!({"id":"custom","version":"1"}),
                origin(Some("producer-a")),
            ),
        ] {
            let namespace = isolated_namespace();
            let entry =
                PreparedDiskKey::new(stamp(1), "registry-1", "hook", &authority, &classifier)
                    .unwrap();
            let left = left_behind(&namespace, &entry, &built);
            let stored = fs::metadata(&left).unwrap().len() as usize;
            let successor = PreparedDiskCache::open(
                namespace.clone(),
                &epoch(),
                64 * 1024,
                &reader,
                1 << 20,
                Work::default(),
            )
            .unwrap();
            assert_eq!(successor.decoded_bytes(&entry), None, "{case}");
            assert_eq!(budget(&successor), (0, 0, 0, stored), "{case}");
            assert!(left.exists(), "{case}");
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
        let damages: [(&str, fn(&Path)); 8] = [
            ("a file truncated by one byte", |path: &Path| {
                let length = fs::metadata(path).unwrap().len();
                OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_len(length - 1)
                    .unwrap();
            }),
            ("a file extended by one byte", |path: &Path| {
                OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(b"0")
                    .unwrap();
            }),
            ("a flipped bit in the header binding", |path: &Path| {
                let mut bytes = fs::read(path).unwrap();
                bytes[8] ^= 1;
                fs::write(path, bytes).unwrap();
            }),
            ("a flipped last payload byte", |path: &Path| {
                let mut bytes = fs::read(path).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                fs::write(path, bytes).unwrap();
            }),
            ("a symlink to another regular file", |path: &Path| {
                let decoy = path.with_extension("decoy");
                fs::rename(path, &decoy).unwrap();
                std::os::unix::fs::symlink(&decoy, path).unwrap();
            }),
            ("a second hard link", |path: &Path| {
                fs::hard_link(path, path.with_extension("link")).unwrap();
            }),
            ("a file with mode 0o644", |path: &Path| {
                fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            }),
            ("a removed file", |path: &Path| {
                fs::remove_file(path).unwrap();
            }),
        ];
        for (case, damage) in damages {
            let namespace = isolated_namespace();
            let entry = revision(1);
            let left = left_behind(&namespace, &entry, &built);
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert!(successor.decoded_bytes(&entry).is_some(), "{case}");
            damage(&left);
            assert!(
                matches!(
                    successor.lookup(&entry, &mut DiskRead::default()).unwrap(),
                    DiskLookup::Miss
                ),
                "{case}"
            );
            assert_eq!(successor.decoded_bytes(&entry), None, "{case}");
            assert!(!successor.has_entry(&entry).unwrap(), "{case}");
            assert_eq!(budget(&successor), (0, 0, 0, 0), "{case}");
            assert!(!successor.entry_file(&entry).exists(), "{case}");
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
        let namespace = isolated_namespace();
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &built);
        let successor = reuse_cache(&namespace, 64 * 1024);
        assert!(successor.decoded_bytes(&entry).is_some());
        for (case, other) in [
            ("a different source stamp", key(stamp(2), &authority)),
            (
                "a different authority",
                key(stamp(1), &json!({"root":"/elsewhere"})),
            ),
        ] {
            assert_eq!(successor.decoded_bytes(&other), None, "{case}");
            assert!(
                matches!(
                    successor.lookup(&other, &mut DiskRead::default()).unwrap(),
                    DiskLookup::Miss
                ),
                "{case}"
            );
        }
        assert!(left.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn fresh_facts_replace_an_unverified_inherited_copy() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&second), (1, 0, stored, 0));
        assert!(!second.has_entry(&entry).unwrap());
        assert!(second.insert(&entry, &built, |_| true, |_| true).unwrap());
        let stats = second.stats();
        assert_eq!((stats.entries, stats.retired, stats.writes), (1, 1, 1));
        assert_eq!(budget(&second), (1, stored, 0, 0));
        assert!(second.has_entry(&entry).unwrap());
        assert!(!left.exists());
        assert_eq!(
            second.entry_file(&entry).parent(),
            Some(second.dir.as_path())
        );
        assert!(matches!(
            second.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn live_owner_is_never_claimed() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let live = reuse_cache(&namespace, 64 * 1024);
        assert!(live.insert(&entry, &built, |_| true, |_| true).unwrap());
        let second = reuse_cache(&namespace, 64 * 1024);
        assert!(second.state.lock().unwrap().claimed.is_empty());
        assert_eq!(budget(&second), (0, 0, 0, 0));
        assert_eq!(second.decoded_bytes(&entry), None);
        assert!(matches!(
            live.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        drop(second);
        drop(live);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn inherited_and_garbage_bytes_share_one_budget() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let size = HEADER_BYTES + sonic_rs::to_vec(&built).unwrap().len();
        let cap = 2 * size;
        let first = reuse_cache(&namespace, cap);
        for generation in 1..=2 {
            assert!(first
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap());
        }
        assert_eq!(budget(&first), (2, cap, 0, 0));
        drop(first);
        let dead = dead_owner(&namespace);
        let garbage = planted(
            &dead,
            &EntryName {
                key: [7; 32],
                tag: !engine_tag(&origin(Some("producer-a"))),
                bytes: size,
                accounted: 0,
            }
            .encode(),
            size,
        );
        let second = reuse_cache(&namespace, cap);
        assert_eq!(budget(&second), (2, 0, cap, size));
        assert!(second
            .insert(&revision(3), &built, |_| true, |_| true)
            .unwrap());
        assert_eq!(budget(&second), (2, size, size, 0));
        let stats = second.stats();
        assert_eq!((stats.retired, stats.writes), (1, 1));
        assert!(!garbage.exists());
        assert!(!dead.exists());
        assert!(second.has_entry(&revision(3)).unwrap());
        let kept = (1..=2)
            .filter(|generation| second.decoded_bytes(&revision(*generation)).is_some())
            .count();
        assert_eq!(kept, 1);
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn only_the_three_newest_generations_are_listed() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let size = HEADER_BYTES + sonic_rs::to_vec(&built).unwrap().len();
        for generation in 1..=4 {
            let owner = reuse_cache(&namespace, 64 * 1024);
            assert!(owner
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap());
            assert_eq!(owner.stats().bytes, size);
        }
        let fifth = reuse_cache(&namespace, 64 * 1024);
        let generations: Vec<u64> = fifth
            .state
            .lock()
            .unwrap()
            .claimed
            .iter()
            .map(|owner| owner.generation)
            .collect();
        assert_eq!(generations, [1, 2, 3, 4]);
        assert_eq!(budget(&fifth), (3, 0, 3 * size, size));
        assert_eq!(fifth.decoded_bytes(&revision(1)), None);
        for generation in 2..=4 {
            assert!(
                fifth.decoded_bytes(&revision(generation)).is_some(),
                "generation {generation}"
            );
        }
        drop(fifth);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn sweep_removes_what_is_not_listed_and_then_the_directory() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let listed = revision(1);
        let first = reuse_cache(&namespace, 64 * 1024);
        assert!(first.insert(&listed, &built, |_| true, |_| true).unwrap());
        let dead = first.dir.clone();
        let left = first.entry_file(&listed);
        let stored = first.stats().bytes;
        drop(first);
        let foreign = planted(
            &dead,
            &EntryName {
                key: [7; 32],
                tag: !engine_tag(&origin(Some("producer-a"))),
                bytes: 100,
                accounted: 0,
            }
            .encode(),
            100,
        );
        let twin = planted(
            &dead,
            &EntryName {
                key: listed.name,
                tag: !engine_tag(&origin(Some("producer-a"))),
                bytes: stored,
                accounted: built.accounted_bytes(),
            }
            .encode(),
            stored,
        );
        let temp = planted(&dead, c"ff.tmp", 10);
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&second), (1, 0, stored, 100 + stored));
        assert!(second
            .insert(&revision(2), &built, |_| true, |_| true)
            .unwrap());
        assert!(left.exists());
        assert!(!foreign.exists());
        assert!(!twin.exists());
        assert!(!temp.exists());
        assert_eq!(budget(&second), (2, stored, stored, 0));
        assert!(matches!(
            second.lookup(&listed, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(!left.exists());
        assert!(dead.is_dir());
        assert_eq!(budget(&second), (2, 2 * stored, 0, 0));
        drop(second);
        let third = reuse_cache(&namespace, 64 * 1024);
        assert!(!dead.exists());
        assert_eq!(budget(&third), (2, 0, 2 * stored, 0));
        drop(third);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn claimed_generation_returns_to_the_pool_when_its_claimer_closes() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&second), (1, 0, stored, 0));
        let abandoned = second.dir.clone();
        drop(second);
        assert!(!abandoned.exists());
        assert!(left.exists());
        let third = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&third), (1, 0, stored, 0));
        assert!(matches!(
            third.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(!left.exists());
        drop(third);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn name_whose_decode_estimate_exceeds_the_headroom_is_not_listed() {
        for accounted in [usize::MAX, 1 << 20] {
            let namespace = isolated_namespace();
            drop(reuse_cache(&namespace, 64 * 1024));
            let entry = revision(1);
            let dead = dead_owner(&namespace);
            let oversized = planted(
                &dead,
                &EntryName {
                    key: entry.name,
                    tag: engine_tag(&origin(Some("producer-a"))),
                    bytes: 1024,
                    accounted,
                }
                .encode(),
                1024,
            );
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(
                successor.decoded_bytes(&entry),
                None,
                "accounted {accounted}"
            );
            assert_eq!(budget(&successor), (0, 0, 0, 1024), "accounted {accounted}");
            assert!(oversized.exists(), "accounted {accounted}");
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn directory_under_a_fact_name_is_left_alone_and_rebuilt() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let successor = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&successor), (1, 0, stored, 0));
        fs::remove_file(&left).unwrap();
        fs::create_dir(&left).unwrap();
        assert!(matches!(
            successor.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (0, 0, 0, stored));
        assert!(left.is_dir());
        assert!(!successor.has_entry(&entry).unwrap());
        assert!(successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap());
        assert!(successor.has_entry(&entry).unwrap());
        assert_eq!(
            successor.entry_file(&entry).parent(),
            Some(successor.dir.as_path())
        );
        assert!(left.is_dir());
        assert_eq!(budget(&successor), (1, stored, 0, stored));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn adoption_reclaims_garbage_left_over_budget() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let size = HEADER_BYTES + sonic_rs::to_vec(&built).unwrap().len();
        let first = reuse_cache(&namespace, 2 * size);
        for generation in 1..=2 {
            assert!(first
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap());
        }
        let dead = first.dir.clone();
        let files = [
            first.entry_file(&revision(1)),
            first.entry_file(&revision(2)),
        ];
        drop(first);
        let successor = reuse_cache(&namespace, size);
        assert_eq!(budget(&successor), (1, 0, size, size));
        let (adopted, dropped): (usize, usize) = if successor.decoded_bytes(&revision(1)).is_some()
        {
            (0, 1)
        } else {
            (1, 0)
        };
        let entry = revision(adopted as u64 + 1);
        assert!(successor.decoded_bytes(&entry).is_some());
        assert!(matches!(
            successor.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert_eq!(budget(&successor), (1, size, 0, 0));
        assert!(successor.has_entry(&entry).unwrap());
        assert!(!files[adopted].exists());
        assert!(!files[dropped].exists());
        assert!(!dead.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn nothing_is_published_while_a_claimed_directory_is_unread() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let left = left_behind(&namespace, &revision(1), &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let dead = left.parent().unwrap();
        for index in 0..300u64 {
            planted(
                dead,
                &EntryName {
                    key: Sha256::digest(index.to_le_bytes()).into(),
                    tag: !engine_tag(&origin(Some("producer-a"))),
                    bytes: 100,
                    accounted: 0,
                }
                .encode(),
                100,
            );
        }
        let successor = reuse_cache(&namespace, 1 << 20);
        assert_eq!(budget(&successor), (1, 0, stored, 300 * 100));
        successor.state.lock().unwrap().claimed[0].listed = false;
        let entry = revision(2);
        assert!(!successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap());
        assert_eq!(successor.stats().writes, 0);
        assert!(!successor.has_entry(&entry).unwrap());
        assert!(!successor.state.lock().unwrap().claimed[0].listed);
        assert!(successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap());
        assert_eq!(successor.stats().writes, 1);
        assert!(successor.has_entry(&entry).unwrap());
        assert!(successor.state.lock().unwrap().claimed[0].listed);
        assert!(left.exists());
        assert_eq!(budget(&successor), (2, stored, stored, 0));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn listing_leaves_room_for_the_index_it_builds() {
        let exact = (1 << 20) - 1024 - decode_transient_bytes(1024 - HEADER_BYTES);
        for (case, accounted, expected) in [
            ("an estimate equal to the headroom", exact, (0, 0, 0, 1024)),
            ("an estimate inside the headroom", 4096, (1, 0, 1024, 0)),
        ] {
            let namespace = isolated_namespace();
            drop(reuse_cache(&namespace, 64 * 1024));
            let entry = revision(1);
            let dead = dead_owner(&namespace);
            planted(
                &dead,
                &EntryName {
                    key: entry.name,
                    tag: engine_tag(&origin(Some("producer-a"))),
                    bytes: 1024,
                    accounted,
                }
                .encode(),
                1024,
            );
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(
                successor.decoded_bytes(&entry).is_some(),
                expected.0 == 1,
                "{case}"
            );
            assert_eq!(budget(&successor), expected, "{case}");
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn refused_insert_leaves_the_cache_alone() {
        let namespace = isolated_namespace();
        let small = pushed_facts(5, 5);
        let large = pushed_facts(9, 9);
        let left = left_behind(&namespace, &revision(1), &small);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let larger = HEADER_BYTES + sonic_rs::to_vec(&large).unwrap().len();
        assert!(larger > stored, "{larger} against {stored}");
        let successor = reuse_cache(&namespace, stored + larger - 1);
        assert_eq!(budget(&successor), (1, 0, stored, 0));
        fs::remove_file(&left).unwrap();
        fs::create_dir(&left).unwrap();
        assert!(matches!(
            successor
                .lookup(&revision(1), &mut DiskRead::default())
                .unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (0, 0, 0, stored));
        let kept = revision(2);
        assert!(successor.insert(&kept, &small, |_| true, |_| true).unwrap());
        let before = successor.stats();
        assert!(!successor
            .insert(&revision(3), &large, |_| true, |_| true)
            .unwrap());
        let after = successor.stats();
        assert_eq!(
            (after.retired, after.writes),
            (before.retired, before.writes)
        );
        assert!(successor.has_entry(&kept).unwrap());
        assert!(successor.entry_file(&kept).exists());
        assert_eq!(budget(&successor), (1, stored, 0, stored));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn stranded_eviction_refuses_the_insert() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let left = left_behind(&namespace, &revision(1), &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let successor = reuse_cache(&namespace, 2 * stored - 1);
        assert_eq!(budget(&successor), (1, 0, stored, 0));
        fs::remove_file(&left).unwrap();
        fs::create_dir(&left).unwrap();
        let entry = revision(2);
        assert!(!successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap());
        assert_eq!(budget(&successor), (0, 0, 0, stored));
        let stats = successor.stats();
        assert_eq!((stats.retired, stats.writes), (1, 0));
        assert!(!successor.has_entry(&entry).unwrap());
        assert!(left.is_dir());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn adoption_restarts_a_paused_sweep_pass() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let first = reuse_cache(&namespace, 1 << 20);
        for generation in 1..=48 {
            assert!(first
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap());
        }
        let waiting = first.entry_file(&revision(2));
        let stored = fs::metadata(&waiting).unwrap().len() as usize;
        drop(first);
        let successor = reuse_cache(&namespace, 1 << 20);
        assert_eq!(budget(&successor), (48, 0, 48 * stored, 0));
        assert!(successor
            .insert(&revision(49), &built, |_| true, |_| true)
            .unwrap());
        assert!(successor.state.lock().unwrap().claimed[0].cursor.is_some());
        assert!(!successor.state.lock().unwrap().claimed[0].dirty);
        assert!(matches!(
            successor
                .lookup(&revision(1), &mut DiskRead::default())
                .unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(successor.state.lock().unwrap().claimed[0].dirty);
        let mut inserts = 0u64;
        while !successor.state.lock().unwrap().claimed[0].swept {
            assert!(inserts < 64, "the restarted pass never settled");
            assert!(successor
                .insert(&revision(50 + inserts), &built, |_| true, |_| true)
                .unwrap());
            inserts += 1;
        }
        assert_eq!(budget(&successor).3, 0);
        assert!(successor.decoded_bytes(&revision(2)).is_some());
        assert!(waiting.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn vanished_claimed_directory_settles_as_empty() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let lost = revision(1);
        let left = left_behind(&namespace, &lost, &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let successor = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&successor), (1, 0, stored, 0));
        fs::remove_dir_all(left.parent().unwrap()).unwrap();
        assert!(successor
            .insert(&revision(2), &built, |_| true, |_| true)
            .unwrap());
        let settled = {
            let state = successor.state.lock().unwrap();
            let owner = &state.claimed[0];
            (owner.swept, owner.listed, owner.garbage)
        };
        assert_eq!(settled, (true, true, 0));
        assert!(matches!(
            successor.lookup(&lost, &mut DiskRead::default()).unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (1, stored, 0, 0));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn store_without_a_producer_keeps_the_legacy_file_name() {
        let built = facts();
        let entry = revision(1);
        let plain = cache(64 * 1024);
        assert!(plain.insert(&entry, &built, |_| true, |_| true).unwrap());
        let legacy = format!("{:x}.facts", Sha256::digest(entry.digest));
        let written = plain.entry_file(&entry);
        assert_eq!(written.file_name().unwrap().to_str().unwrap(), legacy);
        let mut held: Vec<String> = fs::read_dir(&plain.dir)
            .unwrap()
            .map(|listed| listed.unwrap().file_name().into_string().unwrap())
            .collect();
        held.sort();
        assert_eq!(held, [legacy, "owner.lock".to_owned()]);
        assert!(matches!(
            plain.lookup(&entry, &mut DiskRead::default()).unwrap(),
            DiskLookup::Hit(_)
        ));
        let namespace = isolated_namespace();
        let reusing = reuse_cache(&namespace, 64 * 1024);
        assert!(reusing.insert(&entry, &built, |_| true, |_| true).unwrap());
        let named = reusing.entry_file(&entry);
        let name = CString::new(named.file_name().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(name.to_bytes().len(), 118);
        assert!(EntryName::parse(&name).is_some());
        drop(reusing);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn entry_name_round_trips() {
        let name = EntryName {
            key: std::array::from_fn(|index| index as u8),
            tag: 0x0123_4567_89ab_cdef,
            bytes: 4096,
            accounted: 2048,
        };
        let encoded = name.encode();
        let text = encoded.to_str().unwrap();
        assert_eq!(
            text,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
             0123456789abcdef00000000000010000000000000000800.facts"
        );
        assert_eq!(text.len(), ENTRY_NAME_BYTES);
        assert!(generated_entry(&encoded));
        let parsed = EntryName::parse(&encoded).unwrap();
        assert_eq!(
            (parsed.key, parsed.tag, parsed.bytes, parsed.accounted),
            (name.key, name.tag, name.bytes, name.accounted)
        );
        for (case, rejected) in [
            ("one byte longer", format!("0{text}")),
            ("one byte shorter", text[1..].to_owned()),
            ("the wrong suffix", format!("{}.fact0", &text[..112])),
            ("an uppercase body", text.replacen('a', "A", 1)),
            ("a non-hex body", format!("g{}", &text[1..])),
        ] {
            assert!(
                EntryName::parse(&CString::new(rejected).unwrap()).is_none(),
                "{case}"
            );
        }
    }
}

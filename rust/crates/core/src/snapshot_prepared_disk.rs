use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

use sha2::{Digest, Sha256};
use sonic_rs::{json, JsonValueTrait, Value};

use crate::snapshot::{
    Cancellation, SnapshotError, SourceIdentity, SourceStamp, Status, PARSER_VERSION,
};
#[cfg(test)]
use crate::snapshot_ledger::Reserved;
use crate::snapshot_ledger::{Table, Work};
use crate::snapshot_memory::sonic_node_buffer_bytes;
use crate::snapshot_prepared::{
    DecodedIndexSection, FactsNeed, OverrideEvent, OverrideFacts, PreparedFacts, TextState,
};

const VERSION: &[u8; 8] = b"CTPF0005";
pub(crate) const HEADER_BYTES: usize = 144;
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
const ENTRY_NAME_BYTES: usize = 166;
const LEGACY_ENTRY_NAME_BYTES: [usize; 2] = [150, 118];
const MAX_STRANDED_OWNED: usize = 64;
const READ_CHUNK_BYTES: usize = 64 * 1024;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpgradeStep {
    Staged,
    Published,
}

#[cfg(test)]
pub(crate) type UpgradeHook = Box<dyn FnMut(UpgradeStep, &CStr, &CStr) + Send>;

#[cfg(test)]
static UPGRADE_HOOKS: LazyLock<Mutex<HashMap<CString, UpgradeHook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn run_upgrade_hook(owner_name: &CStr, step: UpgradeStep, fresh: &CStr, old: &CStr) {
    if let Some(hook) = UPGRADE_HOOKS
        .lock()
        .expect("upgrade hooks")
        .get_mut(owner_name)
    {
        hook(step, fresh, old);
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

fn read_chunks(
    file: &std::fs::File,
    wanted: usize,
    bytes: &mut Vec<u8>,
    mut checkpoint: impl FnMut() -> Result<(), SnapshotError>,
) -> Result<(), SnapshotError> {
    let mut unread = file.take(wanted as u64);
    while (&mut unread)
        .take(READ_CHUNK_BYTES as u64)
        .read_to_end(bytes)
        .map_err(disk_error)?
        == READ_CHUNK_BYTES
    {
        checkpoint()?;
    }
    Ok(())
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

fn legacy_entry_bytes(name: &CStr) -> Option<usize> {
    let bytes = name.to_bytes();
    if !LEGACY_ENTRY_NAME_BYTES.contains(&bytes.len())
        || !bytes.ends_with(b".facts")
        || !hex_bytes(&bytes[..bytes.len() - 6])
    {
        return None;
    }
    usize::from_str_radix(std::str::from_utf8(&bytes[80..96]).ok()?, 16).ok()
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
    entries.failure().is_none()
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
    pub engine: &'a str,
    pub max_source_bytes: usize,
    pub max_entry_bytes: usize,
}

impl FactOrigin<'_> {
    fn compatibility(&self, binding: &str) -> Result<[u8; 32], SnapshotError> {
        let identity = json!({
            "version": "prepared-facts/2",
            "format": std::str::from_utf8(VERSION).expect("ascii format version"),
            "parser": PARSER_VERSION,
            "producer": self.producer,
            "engine": self.engine,
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
    index_len: u64,
    text_len: u64,
    index_accounted: u64,
    accounted: u64,
    index_seal: [u8; 32],
    text_seal: [u8; 32],
    text_state: TextState,
    scope: FactsNeed,
}

impl EntryHeader {
    fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..8].copy_from_slice(VERSION);
        bytes[8..40].copy_from_slice(&self.binding);
        bytes[40..48].copy_from_slice(&self.index_len.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.text_len.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.index_accounted.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.accounted.to_le_bytes());
        bytes[72..104].copy_from_slice(&self.index_seal);
        bytes[104..136].copy_from_slice(&self.text_seal);
        bytes[136] = self.text_state as u8;
        bytes[137] = self.scope as u8;
        bytes
    }

    fn decode(bytes: &[u8; HEADER_BYTES]) -> Option<Self> {
        if bytes[..8] != *VERSION {
            return None;
        }
        let text_state = TextState::from_raw(u64::from(bytes[136]))?;
        let scope = FactsNeed::from_raw(u64::from(bytes[137]))?;
        if (scope == FactsNeed::Full) != (text_state != TextState::Unbuilt) {
            return None;
        }
        Some(Self {
            binding: bytes[8..40].try_into().expect("binding header"),
            index_len: u64::from_le_bytes(bytes[40..48].try_into().expect("index length header")),
            text_len: u64::from_le_bytes(bytes[48..56].try_into().expect("text length header")),
            index_accounted: u64::from_le_bytes(
                bytes[56..64].try_into().expect("index accounted header"),
            ),
            accounted: u64::from_le_bytes(bytes[64..72].try_into().expect("accounted header")),
            index_seal: bytes[72..104].try_into().expect("index seal header"),
            text_seal: bytes[104..136].try_into().expect("text seal header"),
            text_state,
            scope,
        })
    }
}

struct EntryName {
    key: [u8; 32],
    tag: u64,
    bytes: usize,
    accounted: usize,
    index_bytes: usize,
    index_accounted: usize,
    text_state: u64,
    scope: FactsNeed,
}

impl EntryName {
    fn encode(&self) -> CString {
        let high = u128::from_be_bytes(self.key[..16].try_into().expect("entry name key"));
        let low = u128::from_be_bytes(self.key[16..].try_into().expect("entry name key"));
        CString::new(format!(
            "{:032x}{:032x}{:016x}{:016x}{:016x}{:016x}{:016x}{:016x}.facts",
            high,
            low,
            self.tag,
            self.bytes,
            self.accounted,
            self.index_bytes,
            self.index_accounted,
            self.text_state | ((self.scope as u64) << 8)
        ))
        .expect("entry file name")
    }

    fn parse(name: &CStr) -> Option<Self> {
        let bytes = name.to_bytes();
        if bytes.len() != ENTRY_NAME_BYTES
            || !bytes.ends_with(b".facts")
            || !hex_bytes(&bytes[..160])
        {
            return None;
        }
        let text = std::str::from_utf8(&bytes[..160]).ok()?;
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
            index_bytes: usize::from_str_radix(&text[112..128], 16).ok()?,
            index_accounted: usize::from_str_radix(&text[128..144], 16).ok()?,
            text_state: u64::from_str_radix(&text[144..160], 16).ok()? & 255,
            scope: FactsNeed::from_raw(u64::from_str_radix(&text[144..160], 16).ok()? >> 8)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PreparedDiskKey {
    identity: SourceIdentity,
    digest: [u8; 32],
    name: [u8; 32],
    native: bool,
}

impl PreparedDiskKey {
    pub(super) fn identity_range(
        identity: SourceIdentity,
    ) -> (std::ops::Bound<Self>, std::ops::Bound<Self>) {
        (
            std::ops::Bound::Included(Self {
                identity,
                digest: [0; 32],
                name: [0; 32],
                native: false,
            }),
            std::ops::Bound::Included(Self {
                identity,
                digest: [255; 32],
                name: [255; 32],
                native: true,
            }),
        )
    }

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
            identity: stamp.identity,
            digest,
            name: Sha256::digest(digest).into(),
            native: classifier["id"].as_str() == Some("native")
                && classifier["version"].as_str() == Some("1"),
        })
    }

    pub fn identity(&self) -> SourceIdentity {
        self.identity
    }
}

pub enum DiskLookup {
    Hit(PreparedFacts),
    IndexOnly(PreparedFacts),
    SkillsOnly(PreparedFacts),
    Changed,
    Miss,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
    Written,
    Listed,
    AlreadyFull,
    Refused,
}

impl Inserted {
    pub fn serves(self) -> bool {
        !matches!(self, Self::Refused)
    }
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
    index_bytes: usize,
    index_accounted: usize,
    text_state: TextState,
    scope: FactsNeed,
    last_used: u64,
}

impl DiskEntry {
    fn text_bytes(&self) -> usize {
        self.bytes - HEADER_BYTES - self.index_bytes
    }

    fn read_scope(&self, need: FactsNeed) -> FactsNeed {
        need.min(self.scope)
    }

    fn read_bytes(&self, need: FactsNeed) -> usize {
        match need {
            FactsNeed::Skills | FactsNeed::Index => HEADER_BYTES + self.index_bytes,
            FactsNeed::Full => probed_read_bytes(self.bytes),
        }
    }

    fn accounted_for(&self, need: FactsNeed) -> usize {
        match self.read_scope(need) {
            FactsNeed::Skills | FactsNeed::Index => self.index_accounted,
            FactsNeed::Full => self.accounted,
        }
    }

    fn decoded_bytes(&self, need: FactsNeed) -> usize {
        match self.read_scope(need) {
            FactsNeed::Skills | FactsNeed::Index => {
                HEADER_BYTES
                    + self.index_bytes
                    + self.index_accounted
                    + decode_transient_bytes(self.index_bytes)
            }
            FactsNeed::Full => {
                self.bytes
                    + self.accounted
                    + decode_transient_bytes(self.index_bytes)
                    + decode_transient_bytes(self.text_bytes())
            }
        }
    }
}

struct Replaced {
    name: CString,
    bytes: usize,
    rank: Option<usize>,
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
    stranded_owned: Vec<(CString, usize)>,
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
            stranded_owned: Vec::new(),
            clock: MAX_CLAIMED_OWNERS as u64,
            writes: 0,
            write_bytes: 0,
        }
    }

    fn claimed_garbage_bytes(&self) -> usize {
        self.claimed
            .iter()
            .fold(0, |sum, owner| sum.saturating_add(owner.garbage))
    }

    fn stranded_bytes(&self) -> usize {
        self.stranded_owned
            .iter()
            .fold(0, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }

    fn garbage_bytes(&self) -> usize {
        self.claimed_garbage_bytes()
            .saturating_add(self.stranded_bytes())
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
    #[cfg(test)]
    pub(crate) dir_checks: AtomicUsize,
    #[cfg(test)]
    pub(crate) list_hook: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
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
            #[cfg(test)]
            dir_checks: AtomicUsize::new(0),
            #[cfg(test)]
            list_hook: Mutex::new(None),
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
        #[cfg(test)]
        self.dir_checks.fetch_add(1, Ordering::Relaxed);
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
            if !generated_entry(&entry_name)
                && legacy_entry_bytes(&entry_name).is_none()
                && EntryName::parse(&entry_name).is_none()
            {
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
        let mut refused: HashMap<[u8; 32], (FactsNeed, TextState, usize)> = HashMap::new();
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
                    if let Some(bytes) = legacy_entry_bytes(&name) {
                        claimed[rank].garbage = claimed[rank]
                            .garbage
                            .saturating_add(bytes.min(self.max_bytes));
                    }
                    continue;
                };
                let index_growth = state.entries.growth_for(&parsed.key);
                let text_state = TextState::from_raw(parsed.text_state);
                let well_formed = (HEADER_BYTES..=self.max_bytes).contains(&parsed.bytes)
                    && parsed.index_bytes <= parsed.bytes - HEADER_BYTES
                    && parsed.index_accounted <= parsed.accounted
                    && text_state.is_some_and(|text_state| {
                        (parsed.scope == FactsNeed::Full) == (text_state != TextState::Unbuilt)
                            && (text_state != TextState::Unbuilt
                                || parsed.bytes == HEADER_BYTES + parsed.index_bytes
                                    && parsed.accounted == parsed.index_accounted)
                    });
                let standing = state.entries.get(&parsed.key).map(|listed| {
                    (
                        listed.last_used as usize,
                        listed.bytes,
                        (listed.scope, listed.text_state, listed.bytes),
                    )
                });
                let displaced_bytes = standing.map_or(0, |(_, bytes, _)| bytes);
                let wins = well_formed
                    && parsed.tag == engine_tag
                    && newest - claimed[rank].generation < MAX_INHERITED_GENERATIONS
                    && text_state.is_some_and(|text_state| {
                        let candidate = (parsed.scope, text_state, parsed.bytes);
                        standing.is_none_or(|(_, _, standing)| candidate > standing)
                            && refused
                                .get(&parsed.key)
                                .is_none_or(|refused| candidate > *refused)
                    });
                let decoded = wins
                    .then(|| decode_transient_bytes(parsed.bytes - HEADER_BYTES))
                    .and_then(|transient| parsed.accounted.checked_add(parsed.bytes + transient))
                    .filter(|decoded| {
                        parsed.bytes <= self.max_bytes - (state.inherited_bytes - displaced_bytes)
                            && (standing.is_some() || state.entries.len() < MAX_INHERITED_ENTRIES)
                            && growth
                                .saturating_add(index_growth)
                                .saturating_add(largest.max(*decoded))
                                <= headroom
                    });
                match decoded {
                    Some(decoded) => {
                        if let Some((loser_rank, loser_bytes, _)) = standing {
                            claimed[loser_rank].garbage = claimed[loser_rank]
                                .garbage
                                .saturating_add(loser_bytes.min(self.max_bytes));
                            state.inherited_bytes -= loser_bytes;
                        }
                        growth += index_growth;
                        largest = largest.max(decoded);
                        state.entries.reserve_for(&parsed.key);
                        state.entries.insert(
                            parsed.key,
                            DiskEntry {
                                bytes: parsed.bytes,
                                accounted: parsed.accounted,
                                index_bytes: parsed.index_bytes,
                                index_accounted: parsed.index_accounted,
                                text_state: text_state.expect("well-formed entry"),
                                scope: parsed.scope,
                                last_used: rank as u64,
                            },
                        );
                        state.inherited_bytes += parsed.bytes;
                    }
                    None => {
                        if wins {
                            if let Some((loser_rank, loser_bytes, _)) = standing {
                                claimed[loser_rank].garbage = claimed[loser_rank]
                                    .garbage
                                    .saturating_add(loser_bytes.min(self.max_bytes));
                                state.inherited_bytes -= loser_bytes;
                                state.entries.remove(&parsed.key);
                            }
                            refused.insert(
                                parsed.key,
                                (
                                    parsed.scope,
                                    text_state.expect("well-formed entry"),
                                    parsed.bytes,
                                ),
                            );
                        }
                        claimed[rank].garbage = claimed[rank]
                            .garbage
                            .saturating_add(parsed.bytes.min(self.max_bytes));
                    }
                }
            }
            claimed[rank].listed = names.failure().is_none();
        }
        if claimed.iter().any(|owner| !owner.listed) {
            let provisional: Vec<[u8; 32]> = state.entries.iter().map(|(name, _)| *name).collect();
            for name in provisional {
                let entry = state.entries.remove(&name).expect("provisional entry");
                let rank = entry.last_used as usize;
                state.inherited_bytes -= entry.bytes;
                claimed[rank].garbage = claimed[rank]
                    .garbage
                    .saturating_add(entry.bytes.min(self.max_bytes));
            }
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
        EntryName {
            key: key.name,
            tag: tag(&self.compatibility(key)),
            bytes: entry.bytes,
            accounted: entry.accounted,
            index_bytes: entry.index_bytes,
            index_accounted: entry.index_accounted,
            text_state: entry.text_state as u64,
            scope: entry.scope,
        }
        .encode()
    }

    fn entry_names(&self, name: &[u8; 32], entry: &DiskEntry) -> Vec<CString> {
        [Some(self.owner_compatibility), self.engine_compatibility]
            .iter()
            .flatten()
            .map(|compatibility| {
                EntryName {
                    key: *name,
                    tag: tag(compatibility),
                    bytes: entry.bytes,
                    accounted: entry.accounted,
                    index_bytes: entry.index_bytes,
                    index_accounted: entry.index_accounted,
                    text_state: entry.text_state as u64,
                    scope: entry.scope,
                }
                .encode()
            })
            .collect()
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
            index_bytes: 0,
            index_accounted: 0,
            text_state: 0,
            scope: FactsNeed::Index,
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
        let candidates = self.entry_names(&name, entry);
        let written = if inherited {
            &candidates[1..]
        } else {
            &candidates[..]
        };
        let mut stranded = false;
        for candidate in written {
            match unlinkat_file(dir_fd, candidate, 0) {
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
            .saturating_add(state.claimed_garbage_bytes())
            .saturating_add(size)
            > self.max_bytes
    }

    fn make_room(
        &self,
        state: &mut DiskState,
        size: usize,
        pinned: Option<&[u8; 32]>,
    ) -> Result<bool, SnapshotError> {
        let mut names = ROOM_SWEEP_NAMES;
        self.retry_stranded(state, &mut names);
        if state.unread() || (state.garbage_bytes() > 0 && self.over_budget(state, size)) {
            self.sweep(state, names);
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
                .filter(|(name, _)| pinned != Some(*name))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(name, _)| *name);
            match oldest {
                Some(name) => self.retire(state, name)?,
                None if pinned.is_some() => return Ok(false),
                None => return Err(incomplete("prepared facts disk cache capacity exhausted")),
            }
        }
        Ok(!unread)
    }

    fn retry_stranded(&self, state: &mut DiskState, remaining: &mut usize) {
        let mut index = 0;
        while *remaining > 0 && index < state.stranded_owned.len() {
            *remaining -= 1;
            let unlinked = match self.unlink_entry(&state.stranded_owned[index].0) {
                Ok(()) => true,
                Err(error) => error.kind() == io::ErrorKind::NotFound,
            };
            if unlinked {
                let (_, bytes) = state.stranded_owned.remove(index);
                state.bytes -= bytes;
            } else {
                index += 1;
            }
        }
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
                                && entry.index_bytes == parsed.index_bytes
                                && entry.index_accounted == parsed.index_accounted
                                && entry.text_state as u64 == parsed.text_state
                                && entry.scope == parsed.scope
                        });
                    if kept {
                        continue;
                    }
                    parsed.bytes.min(self.max_bytes)
                }
                None => match legacy_entry_bytes(&name) {
                    Some(bytes) => bytes.min(self.max_bytes),
                    None if generated_entry(&name) => 0,
                    None => continue,
                },
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
        self.retry_stranded(state, &mut remaining);
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
        need: FactsNeed,
        admitted: usize,
        cancel: &Cancellation,
        deadline_unix_ms: u64,
    ) -> Result<DiskLookup, SnapshotError> {
        cancel.check(deadline_unix_ms)?;
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        cancel.check(deadline_unix_ms)?;
        let Some(entry) = state.entries.get(&key.name) else {
            return Ok(DiskLookup::Miss);
        };
        if entry.decoded_bytes(need) != admitted {
            return Ok(DiskLookup::Changed);
        }
        let (expected, index_bytes, last_used) = (entry.bytes, entry.index_bytes, entry.last_used);
        let (accounted, index_accounted) = (entry.accounted, entry.index_accounted);
        let (wanted, accounted_bound) = (entry.read_bytes(need), entry.accounted_for(need));
        let (text_state, served, scope) = (entry.text_state, entry.read_scope(need), entry.scope);
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
            let checkpoint = || {
                #[cfg(test)]
                run_read_hook(&self.owner_name);
                cancel.check(deadline_unix_ms)
            };
            checkpoint()?;
            let mut bytes = Vec::with_capacity(wanted.min(expected));
            read.operations += 1;
            let finished = read_chunks(&file, wanted, &mut bytes, checkpoint);
            read.bytes += bytes.len() as u64;
            finished?;
            if need != FactsNeed::Full
                && file.metadata().map_err(disk_error)?.len() != expected as u64
            {
                return Err(incomplete("prepared facts cache entry changed"));
            }
            cancel.check(deadline_unix_ms)?;
            let index_end = HEADER_BYTES + index_bytes;
            let verified = bytes
                .get(..HEADER_BYTES)
                .and_then(|header| <&[u8; HEADER_BYTES]>::try_from(header).ok())
                .and_then(EntryHeader::decode)
                .is_some_and(|header| {
                    header.binding == self.binding(key)
                        && header.index_len == index_bytes as u64
                        && header.text_len == (expected - index_end) as u64
                        && header.index_accounted == index_accounted as u64
                        && header.accounted == accounted as u64
                        && header.text_state == text_state
                        && header.scope == scope
                        && match served {
                            FactsNeed::Skills | FactsNeed::Index => {
                                bytes.len() == index_end
                                    && header.index_seal
                                        == seal(header.index_accounted, &bytes[HEADER_BYTES..])
                            }
                            FactsNeed::Full => {
                                bytes.len() == expected
                                    && header.index_seal
                                        == seal(
                                            header.index_accounted,
                                            &bytes[HEADER_BYTES..index_end],
                                        )
                                    && header.text_seal
                                        == seal(header.accounted, &bytes[index_end..])
                            }
                        }
                });
            if !verified {
                return Err(incomplete("prepared facts cache integrity check failed"));
            }
            cancel.check(deadline_unix_ms)?;
            let index: DecodedIndexSection = sonic_rs::from_slice(&bytes[HEADER_BYTES..index_end])
                .map_err(|_| incomplete("prepared facts cache payload invalid"))?;
            let overrides = match served {
                FactsNeed::Skills | FactsNeed::Index => OverrideFacts::Unloaded,
                FactsNeed::Full => {
                    cancel.check(deadline_unix_ms)?;
                    let section: Option<Vec<OverrideEvent>> =
                        sonic_rs::from_slice(&bytes[index_end..])
                            .map_err(|_| incomplete("prepared facts cache payload invalid"))?;
                    OverrideFacts::from_section(section)
                }
            };
            let facts = PreparedFacts::from_sections(index, overrides);
            if facts.capability()
                != scope.min(if served == FactsNeed::Full {
                    FactsNeed::Full
                } else {
                    FactsNeed::Index
                })
            {
                return Err(incomplete("prepared facts capability differs"));
            }
            cancel.check(deadline_unix_ms)?;
            if inherited {
                if facts.accounted_bytes() > accounted_bound {
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
                    let _ = self.make_room(&mut state, 0, None);
                }
                self.sweep(&mut state, INSERT_SWEEP_NAMES);
                Ok(if facts.serves(need) {
                    DiskLookup::Hit(facts)
                } else if facts.serves(FactsNeed::Index) {
                    DiskLookup::IndexOnly(facts)
                } else {
                    DiskLookup::SkillsOnly(facts)
                })
            }
            Err(error) if matches!(error.status, Status::Cancelled | Status::Deadline) => {
                Err(error)
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
        Ok(self.lists(key))
    }

    pub fn lists(&self, key: &PreparedDiskKey) -> bool {
        self.lists_for(key, FactsNeed::Skills)
    }

    pub fn lists_for(&self, key: &PreparedDiskKey, need: FactsNeed) -> bool {
        let listed = self
            .state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .is_some_and(|entry| {
                entry.last_used >= MAX_CLAIMED_OWNERS as u64 && entry.scope >= need
            });
        #[cfg(test)]
        {
            let hook = self.list_hook.lock().expect("list hook").clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        listed
    }

    pub fn touch(
        &self,
        key: &PreparedDiskKey,
        need: FactsNeed,
        unread: impl FnOnce(usize) -> bool,
    ) -> Result<bool, SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        let owned = state
            .entries
            .get(&key.name)
            .filter(|entry| entry.last_used >= MAX_CLAIMED_OWNERS as u64 && entry.scope >= need)
            .map(|entry| entry.accounted_for(need));
        if !owned.is_some_and(unread) {
            return Ok(false);
        }
        state.clock += 1;
        let clock = state.clock;
        state
            .entries
            .get_mut(&key.name)
            .expect("cached entry")
            .last_used = clock;
        self.sweep(&mut state, INSERT_SWEEP_NAMES);
        Ok(true)
    }

    pub fn decoded_bytes(&self, key: &PreparedDiskKey, need: FactsNeed) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .map(|entry| entry.decoded_bytes(need))
    }

    pub fn read_bound(&self, key: &PreparedDiskKey, need: FactsNeed) -> Option<usize> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .map(|entry| entry.read_bytes(need))
    }

    #[cfg(test)]
    pub(crate) fn entry_index_bytes(&self, key: &PreparedDiskKey) -> usize {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .expect("cached entry")
            .index_bytes
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
    pub(crate) fn entry_text_state(&self, key: &PreparedDiskKey) -> Option<TextState> {
        self.state
            .lock()
            .expect("prepared facts disk state")
            .entries
            .get(&key.name)
            .map(|entry| entry.text_state)
    }

    #[cfg(test)]
    pub(crate) fn set_upgrade_hook(&self, hook: Option<UpgradeHook>) {
        let mut hooks = UPGRADE_HOOKS.lock().expect("upgrade hooks");
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
    ) -> Result<Inserted, SnapshotError> {
        self.check_dir()?;
        let mut state = self.state.lock().expect("prepared facts disk state");
        let text_state = facts.text_state();
        let listed = state.entries.get(&key.name).map(|entry| {
            (
                entry.last_used >= MAX_CLAIMED_OWNERS as u64,
                entry.scope,
                entry.text_state,
            )
        });
        let replaced = match listed {
            Some((_, scope, _)) if facts.capability() < scope => return Ok(Inserted::Listed),
            Some((_, scope, _)) if facts.capability() > scope => {
                let entry = state.entries.get(&key.name).expect("listed entry");
                Some(Replaced {
                    name: self.entry_name(key, entry),
                    bytes: entry.bytes,
                    rank: (entry.last_used < MAX_CLAIMED_OWNERS as u64)
                        .then_some(entry.last_used as usize),
                })
            }
            Some((_, FactsNeed::Full, TextState::Present | TextState::Exceeded)) => {
                return Ok(if facts.is_full() {
                    Inserted::AlreadyFull
                } else {
                    Inserted::Listed
                });
            }
            Some((true, _, _)) => return Ok(Inserted::Listed),
            Some((false, _, _)) => {
                self.retire(&mut state, key.name)?;
                None
            }
            None => None,
        };
        let limit = self.max_bytes - HEADER_BYTES;
        let index_section = facts.index_section();
        let text_section = match text_state {
            TextState::Unbuilt => None,
            TextState::Present | TextState::Exceeded => Some(facts.text_section()),
        };
        let mut counted = crate::snapshot_codec::Counter { bytes: 0, limit };
        crate::snapshot_codec::write_json(&mut counted, &index_section, limit)
            .map_err(|_| incomplete("prepared facts disk cache capacity exhausted"))?;
        let index_len = counted.bytes;
        if let Some(text_section) = text_section {
            crate::snapshot_codec::write_json(&mut counted, &text_section, limit - index_len)
                .map_err(|_| incomplete("prepared facts disk cache capacity exhausted"))?;
        }
        if !reserve(counted.bytes) {
            return Ok(Inserted::Refused);
        }
        let mut payload = Vec::with_capacity(counted.bytes);
        crate::snapshot_codec::write_json(&mut payload, &index_section, index_len)
            .expect("counted prepared index payload");
        assert_eq!(
            payload.len(),
            index_len,
            "prepared index payload outgrew its count"
        );
        if let Some(text_section) = text_section {
            crate::snapshot_codec::write_json(
                &mut payload,
                &text_section,
                counted.bytes - index_len,
            )
            .expect("counted prepared text payload");
        }
        assert_eq!(
            (payload.len(), payload.capacity()),
            (counted.bytes, counted.bytes),
            "prepared facts payload outgrew its count"
        );
        let size = HEADER_BYTES + payload.len();
        let pinned = replaced.as_ref().map(|_| &key.name);
        if !self.make_room(&mut state, size, pinned)? {
            return Ok(Inserted::Refused);
        }
        let strands = 1 + usize::from(replaced.as_ref().is_some_and(|old| old.rank.is_none()));
        if state.stranded_owned.len() + strands > MAX_STRANDED_OWNED {
            return Ok(Inserted::Refused);
        }
        state.clock += 1;
        let temp = CString::new(format!("{:x}.tmp", state.clock)).expect("temp file name");
        let accounted = facts.accounted_bytes();
        let index_accounted = facts.index_accounted_bytes();
        assert!(
            text_state != TextState::Unbuilt || accounted == index_accounted,
            "index-only facts carry text accounting"
        );
        let entry = DiskEntry {
            bytes: size,
            accounted,
            index_bytes: index_len,
            index_accounted,
            text_state,
            scope: facts.capability(),
            last_used: state.clock,
        };
        let name = self.entry_name(key, &entry);
        if state
            .stranded_owned
            .iter()
            .any(|(stranded, _)| *stranded == name)
        {
            return Ok(Inserted::Refused);
        }
        let header = EntryHeader {
            binding: self.binding(key),
            index_len: index_len as u64,
            text_len: (payload.len() - index_len) as u64,
            index_accounted: index_accounted as u64,
            accounted: accounted as u64,
            index_seal: seal(index_accounted as u64, &payload[..index_len]),
            text_seal: seal(accounted as u64, &payload[index_len..]),
            text_state,
            scope: facts.capability(),
        }
        .encode();
        let mut staged = None;
        let result = (|| -> Result<(), SnapshotError> {
            let mut file = self
                .open_entry(&temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)
                .map_err(disk_error)?;
            let mut written = 0;
            let outcome = (|| -> io::Result<()> {
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                for section in [&header[..], &payload[..]] {
                    let mut offset = 0;
                    while offset < section.len() {
                        match file.write(&section[offset..]) {
                            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                            Ok(count) => {
                                offset += count;
                                written += count;
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok(())
            })();
            staged = Some(written);
            outcome.map_err(disk_error)?;
            drop(file);
            #[cfg(test)]
            {
                if let Some(old) = &replaced {
                    run_upgrade_hook(&self.owner_name, UpgradeStep::Staged, &name, &old.name);
                }
            }
            self.rename_entry(&temp, &name).map_err(disk_error)
        })();
        if let Err(error) = result {
            if let Some(staged) = staged {
                self.strand_unless_unlinked(&mut state, temp, staged);
            }
            return Err(error);
        }
        let Some(old) = replaced else {
            if !admit(state.entries.growth_for(&key.name)) {
                self.strand_unless_unlinked(&mut state, name, size);
                return Ok(Inserted::Refused);
            }
            state.bytes += size;
            state.writes += 1;
            state.write_bytes += size as u64;
            state.entries.reserve_for(&key.name);
            state.entries.insert(key.name, entry);
            self.sweep(&mut state, INSERT_SWEEP_NAMES);
            return Ok(Inserted::Written);
        };
        #[cfg(test)]
        run_upgrade_hook(&self.owner_name, UpgradeStep::Published, &name, &old.name);
        state.entries.insert(key.name, entry);
        state.bytes += size;
        state.writes += 1;
        state.write_bytes += size as u64;
        let dir_fd = match old.rank {
            Some(rank) => state.claimed[rank].dir.as_raw_fd(),
            None => self.dir_file.as_raw_fd(),
        };
        let unlinked = match unlinkat_file(dir_fd, &old.name, 0) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        match (old.rank, unlinked) {
            (None, Ok(())) => state.bytes -= old.bytes,
            (None, Err(_)) => state.stranded_owned.push((old.name, old.bytes)),
            (Some(rank), Ok(())) => {
                state.inherited_bytes -= old.bytes;
                let owner = &mut state.claimed[rank];
                if owner.cursor.is_some() {
                    owner.dirty = true;
                }
            }
            (Some(rank), Err(_)) => {
                state.inherited_bytes -= old.bytes;
                let owner = &mut state.claimed[rank];
                owner.garbage = owner.garbage.saturating_add(old.bytes);
                owner.stuck = 0;
                owner.dirty = false;
                owner.cursor = None;
                owner.swept = false;
            }
        }
        self.sweep(&mut state, INSERT_SWEEP_NAMES);
        Ok(Inserted::Written)
    }

    fn strand_unless_unlinked(&self, state: &mut DiskState, name: CString, bytes: usize) {
        match self.unlink_entry(&name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                state.stranded_owned.push((name, bytes));
                state.bytes += bytes;
            }
        }
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
            engine: "engine-a",
            max_source_bytes: 1 << 20,
            max_entry_bytes: 1 << 20,
        }
    }

    fn lookup(
        cache: &PreparedDiskCache,
        key: &PreparedDiskKey,
        read: &mut DiskRead,
        need: FactsNeed,
    ) -> Result<DiskLookup, SnapshotError> {
        cache.lookup(
            key,
            read,
            need,
            cache.decoded_bytes(key, need).unwrap_or(0),
            &Cancellation::default(),
            u64::MAX,
        )
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
        assert!(writer
            .insert(entry, built, |_| true, |_| true)
            .unwrap()
            .serves());
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
            evidence: crate::snapshot_prepared::PreparedEvidence::Index {
                inputs: json!({
                    "calls": [["Read", ["/repo/file.rs"]]],
                    "commands": [],
                    "edited_files": [],
                    "skills": [],
                }),
                has_error: false,
            },
            overrides: OverrideFacts::Loaded(vec![OverrideEvent {
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
        match lookup(&cache, &first, &mut DiskRead::default(), FactsNeed::Full).unwrap() {
            DiskLookup::Hit(loaded) => {
                assert!(loaded.accounted_bytes() >= std::mem::size_of::<PreparedFacts>());
                assert_eq!(loaded.index_inputs(), facts().index_inputs());
                assert_eq!(
                    loaded.text_section().unwrap()[0].text,
                    "override only in source"
                );
            }
            _ => panic!("expected completed facts"),
        }
        cache.insert(&first, &facts(), |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 1);
        assert!(matches!(
            lookup(
                &cache,
                &key(stamp(2), &json!({"root":"/repo"})),
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            lookup(
                &cache,
                &key(stamp(1), &json!({"root":"/elsewhere"})),
                &mut DiskRead::default(),
                FactsNeed::Full
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
                lookup(&cache, &changed, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
                DiskLookup::Miss
            ));
        }
    }

    #[test]
    fn evicted_revision_can_rebuild_in_a_later_event() {
        let sample = facts();
        let cap = HEADER_BYTES + encoded_sections_len(&sample);
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
            lookup(&cache, &first, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            lookup(&cache, &second, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Hit(_)
        ));
        cache.insert(&first, &sample, |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 3);
        assert_eq!(cache.stats().retired, 2);
        assert!(matches!(
            lookup(&cache, &first, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(matches!(
            lookup(&cache, &second, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        assert_eq!(cache.stats().writes, 2);
        assert!(matches!(
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn repeated_evictions_stay_bounded_and_rebuildable() {
        let sample = facts();
        let cap = HEADER_BYTES + encoded_sections_len(&sample);
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
            lookup(
                &cache,
                &key(stamp(0), &json!({"root":"/repo"})),
                &mut DiskRead::default(),
                FactsNeed::Full
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
            lookup(
                &restarted,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
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
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            lookup(&active, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(cache.stats().entries, 0);
        assert!(matches!(
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
    }

    #[test]
    fn lookup_counts_exactly_the_bytes_it_reads() {
        let cache = cache(4096);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let mut missed = DiskRead::default();
        assert!(matches!(
            lookup(&cache, &entry, &mut missed, FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!((missed.operations, missed.bytes), (0, 0));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let stored = cache.entry_file_len(&entry);
        let mut hit = DiskRead::default();
        assert!(matches!(
            lookup(&cache, &entry, &mut hit, FactsNeed::Full).unwrap(),
            DiskLookup::Hit(_)
        ));
        assert_eq!((hit.operations, hit.bytes), (1, stored));
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        let mut corrupt = DiskRead::default();
        assert!(matches!(
            lookup(&cache, &entry, &mut corrupt, FactsNeed::Full).unwrap(),
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
                lookup(&cache, &entry, &mut short, FactsNeed::Full).unwrap(),
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
        let estimates = (
            cache.decoded_bytes(&entry, FactsNeed::Full),
            cache.read_bound(&entry, FactsNeed::Full),
        );
        assert_eq!(estimates.1, Some(cache.entry_file_len(&entry) as usize + 1));
        let displaced = cache.dir.with_extension("displaced");
        fs::rename(&cache.dir, &displaced).unwrap();
        DirBuilder::new().mode(0o700).create(&cache.dir).unwrap();
        assert_eq!(
            (
                cache.decoded_bytes(&entry, FactsNeed::Full),
                cache.read_bound(&entry, FactsNeed::Full)
            ),
            estimates
        );
        let mut read = DiskRead::default();
        let refused = lookup(&cache, &entry, &mut read, FactsNeed::Full)
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
        let bound = cache.read_bound(&entry, FactsNeed::Full).unwrap();
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
        let outcome = lookup(&cache, &entry, &mut read, FactsNeed::Full);
        cache.set_read_hook(None);
        assert!(matches!(outcome.unwrap(), DiskLookup::Retired));
        assert_eq!((read.operations, read.bytes as usize), (1, bound));
        assert_eq!(cache.stats().entries, 0);
    }

    fn chunked_facts() -> PreparedFacts {
        let mut facts = PreparedFacts {
            evidence: crate::snapshot_prepared::PreparedEvidence::Index {
                inputs: json!({
                    "calls": [["Read", ["/repo/file.rs"]]],
                    "commands": [],
                    "edited_files": [],
                    "skills": [],
                }),
                has_error: false,
            },
            overrides: OverrideFacts::Loaded(vec![OverrideEvent {
                text: "x".repeat(3 * READ_CHUNK_BYTES),
                tools: vec!["Read".to_owned()],
            }]),
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
    }

    #[test]
    fn a_stopped_lookup_reads_no_further_and_keeps_the_entry() {
        let cache = cache(2 * 1024 * 1024);
        let entry = revision(1);
        let built = chunked_facts();
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let path = cache.entry_path(entry.digest);
        let stored = fs::read(&path).unwrap();
        assert!(
            stored.len() > 2 * READ_CHUNK_BYTES,
            "entry {}",
            stored.len()
        );
        let admitted = cache.decoded_bytes(&entry, FactsNeed::Full).unwrap();
        for (site, cancelled, cancelled_at_read, deadline_unix_ms, status, counted) in [
            (
                "before the first read",
                false,
                Some(1),
                u64::MAX,
                Status::Cancelled,
                (0, 0),
            ),
            (
                "at a chunk boundary",
                false,
                Some(2),
                u64::MAX,
                Status::Cancelled,
                (1, READ_CHUNK_BYTES as u64),
            ),
            (
                "past the deadline",
                false,
                None,
                0,
                Status::Deadline,
                (0, 0),
            ),
            (
                "cancelled past the deadline",
                true,
                None,
                0,
                Status::Cancelled,
                (0, 0),
            ),
        ] {
            let cancel = Cancellation::default();
            if cancelled {
                cancel.cancel();
            }
            let stop = cancel.clone();
            let mut reads = 0;
            cache.set_read_hook(Some(Box::new(move || {
                reads += 1;
                if Some(reads) == cancelled_at_read {
                    stop.cancel();
                }
            })));
            let mut read = DiskRead::default();
            let stopped = cache
                .lookup(
                    &entry,
                    &mut read,
                    FactsNeed::Full,
                    admitted,
                    &cancel,
                    deadline_unix_ms,
                )
                .err()
                .map(|error| error.status);
            cache.set_read_hook(None);
            assert_eq!(stopped, Some(status), "{site}");
            assert_eq!((read.operations, read.bytes), counted, "{site}");
            assert_eq!(fs::read(&path).unwrap(), stored, "{site}");
            assert_eq!(cache.stats().entries, 1, "{site}");
        }
        let mut read = DiskRead::default();
        let DiskLookup::Hit(loaded) = lookup(&cache, &entry, &mut read, FactsNeed::Full).unwrap()
        else {
            panic!("expected the kept entry to serve");
        };
        assert_eq!((read.operations, read.bytes), (1, stored.len() as u64));
        assert_eq!(
            loaded.text_section().unwrap()[0].text,
            built.text_section().unwrap()[0].text
        );
    }

    #[test]
    fn a_stopped_inherited_lookup_leaves_the_entry_unadopted() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &built);
        let stored = fs::metadata(&left).unwrap().len();
        let successor = reuse_cache(&namespace, 64 * 1024);
        let inherited = budget(&successor);
        assert_eq!(inherited, (1, 0, stored as usize, 0));
        let admitted = successor.decoded_bytes(&entry, FactsNeed::Full).unwrap();
        let cancel = Cancellation::default();
        let stop = cancel.clone();
        successor.set_read_hook(Some(Box::new(move || stop.cancel())));
        let mut read = DiskRead::default();
        let stopped = successor
            .lookup(
                &entry,
                &mut read,
                FactsNeed::Full,
                admitted,
                &cancel,
                u64::MAX,
            )
            .err()
            .map(|error| error.status);
        successor.set_read_hook(None);
        assert_eq!(stopped, Some(Status::Cancelled));
        assert_eq!((read.operations, read.bytes), (0, 0));
        assert_eq!(budget(&successor), inherited);
        assert!(left.exists());
        assert!(!successor.has_entry(&entry).unwrap());
        let DiskLookup::Hit(loaded) =
            lookup(&successor, &entry, &mut read, FactsNeed::Full).unwrap()
        else {
            panic!("expected the inherited entry to serve");
        };
        assert_eq!(loaded.index_inputs(), built.index_inputs());
        assert_eq!((read.operations, read.bytes), (1, stored));
        assert_eq!(budget(&successor), (1, stored as usize, 0, 0));
        assert!(!left.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
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
            evidence: crate::snapshot_prepared::PreparedEvidence::Index {
                inputs: json!({
                    "calls": [["Read", ["/repo/file.rs"]], ["Edit", ["/repo/a.rs", "/repo/b.rs"]]],
                    "commands": ["true"],
                    "edited_files": [{"path": "/repo/a.rs"}, {"path": "/repo/b.rs"}],
                    "skills": [],
                }),
                has_error: false,
            },
            overrides: OverrideFacts::Loaded(override_events),
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

    fn encoded_sections_len(facts: &PreparedFacts) -> usize {
        sonic_rs::to_vec(&facts.index_section()).unwrap().len()
            + sonic_rs::to_vec(&facts.text_section()).unwrap().len()
    }

    fn sections(cache: &PreparedDiskCache, entry: &PreparedDiskKey) -> (usize, usize) {
        let file = cache.entry_file_len(entry) as usize;
        let index = cache.entry_index_bytes(entry);
        (index, file - HEADER_BYTES - index)
    }

    fn index_only(decoded: &PreparedFacts) -> bool {
        decoded.serves(FactsNeed::Index) && matches!(decoded.overrides, OverrideFacts::Unloaded)
    }

    #[test]
    fn decoded_bytes_cover_the_file_and_the_decoded_facts() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert_eq!(cache.decoded_bytes(&entry, FactsNeed::Full), None);
        let built = pushed_facts(5, 5);
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let file = fs::metadata(cache.entry_path(entry.digest)).unwrap().len() as usize;
        let (index, text) = sections(&cache, &entry);
        assert_eq!(
            cache.decoded_bytes(&entry, FactsNeed::Full),
            Some(
                file + built.accounted_bytes()
                    + decode_transients_walk(index)
                    + decode_transients_walk(text)
            )
        );
        assert_eq!(
            cache.decoded_bytes(&entry, FactsNeed::Index),
            Some(
                HEADER_BYTES
                    + index
                    + built.index_accounted_bytes()
                    + decode_transients_walk(index)
            )
        );
        let DiskLookup::Hit(decoded) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
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
            decoded.text_section().unwrap(),
            built.text_section().unwrap(),
        );
        assert_eq!(decoded_events.len(), built_events.len());
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
            evidence: crate::snapshot_prepared::PreparedEvidence::Index {
                inputs: json!({
                    "calls": [["Read", ["/repo/file.rs"]]],
                    "commands": [],
                    "edited_files": [],
                    "skills": [],
                }),
                has_error: false,
            },
            overrides: OverrideFacts::Loaded(vec![OverrideEvent {
                text: "override line\n".repeat(32 * 1024),
                tools: vec!["Read".to_owned()],
            }]),
            accounted: 0,
        };
        built.refresh_accounted();
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let file = fs::metadata(cache.entry_path(entry.digest)).unwrap().len() as usize;
        let (index, text) = sections(&cache, &entry);
        assert!(text >= 400 * 1024, "text section {text}");
        assert!(
            (text - 1) / 2 + 2 >= (3 << 20) / size_of::<Value>(),
            "text section {text} decodes through the thread-local node buffer"
        );
        assert_eq!(
            cache.decoded_bytes(&entry, FactsNeed::Full),
            Some(
                file + built.accounted_bytes()
                    + decode_transients_walk(index)
                    + decode_transients_walk(text)
            )
        );
        let DiskLookup::Hit(decoded) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
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
            &decoded.text_section().unwrap()[0],
            &built.text_section().unwrap()[0],
        );
        assert_eq!(decoded_event.text, built_event.text);
        assert_eq!(decoded_event.text.capacity(), decoded_event.text.len());
        assert_eq!(decoded_event.tools, built_event.tools);
        assert_eq!(decoded.index_inputs(), built.index_inputs());
    }

    #[test]
    fn insert_admits_its_payload_before_allocating_it() {
        let cache = cache(64 * 1024);
        let sample = pushed_facts(3, 2);
        let payload = encoded_sections_len(&sample);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let admissions = std::cell::RefCell::new(Vec::new());
        assert_eq!(
            cache
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
                .unwrap(),
            Inserted::Refused
        );
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
            .unwrap()
            .serves());
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
        let DiskLookup::Hit(decoded) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected completed facts");
        };
        assert_eq!(decoded.index_inputs(), sample.index_inputs());
    }

    #[test]
    fn insert_admits_its_index_growth_before_the_index_grows() {
        let cache = cache(64 * 1024);
        let sample = pushed_facts(1, 1);
        let first = key(stamp(1), &json!({"root":"/repo"}));
        let offered = std::cell::Cell::new(0);
        assert_eq!(
            cache
                .insert(
                    &first,
                    &sample,
                    |_| true,
                    |growth| {
                        offered.set(growth);
                        false
                    }
                )
                .unwrap(),
            Inserted::Refused
        );
        assert!(offered.get() > 0);
        assert!(!cache.has_entry(&first).unwrap());
        assert!(!cache.entry_path(first.digest).exists());
        assert_eq!(cache.index_capacity_bytes(), 0);
        assert_eq!(cache.audit_index_bytes(), 0);
        let stats = cache.stats();
        assert_eq!((stats.entries, stats.bytes, stats.writes), (0, 0, 0));
        assert!(cache
            .insert(&first, &sample, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(cache.index_capacity_bytes(), offered.get());
        assert_eq!(cache.audit_index_bytes(), offered.get());
        assert!(matches!(
            lookup(&cache, &first, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            .unwrap()
            .serves());
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
        assert!(first
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let estimates = (
            first.decoded_bytes(&entry, FactsNeed::Full),
            first.read_bound(&entry, FactsNeed::Full),
        );
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
            (
                second.decoded_bytes(&entry, FactsNeed::Full),
                second.read_bound(&entry, FactsNeed::Full)
            ),
            estimates
        );
        assert!(left.exists());
        let mut read = DiskRead::default();
        let DiskLookup::Hit(loaded) = lookup(&second, &entry, &mut read, FactsNeed::Full).unwrap()
        else {
            panic!("expected inherited facts");
        };
        assert_eq!(loaded.index_inputs(), built.index_inputs());
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
                    engine: "engine-a",
                    max_source_bytes: (1 << 20) + 1,
                    max_entry_bytes: 1 << 20,
                },
            ),
            (
                "a different engine build",
                json!({"id":"native","version":"1"}),
                FactOrigin {
                    producer: Some("producer-a"),
                    engine: "engine-b",
                    max_source_bytes: 1 << 20,
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
            assert_eq!(
                successor.decoded_bytes(&entry, FactsNeed::Full),
                None,
                "{case}"
            );
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
            assert!(
                successor.decoded_bytes(&entry, FactsNeed::Full).is_some(),
                "{case}"
            );
            damage(&left);
            assert!(
                matches!(
                    lookup(
                        &successor,
                        &entry,
                        &mut DiskRead::default(),
                        FactsNeed::Full
                    )
                    .unwrap(),
                    DiskLookup::Miss
                ),
                "{case}"
            );
            assert_eq!(
                successor.decoded_bytes(&entry, FactsNeed::Full),
                None,
                "{case}"
            );
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
        assert!(successor.decoded_bytes(&entry, FactsNeed::Full).is_some());
        for (case, other) in [
            ("a different source stamp", key(stamp(2), &authority)),
            (
                "a different authority",
                key(stamp(1), &json!({"root":"/elsewhere"})),
            ),
        ] {
            assert_eq!(
                successor.decoded_bytes(&other, FactsNeed::Full),
                None,
                "{case}"
            );
            assert!(
                matches!(
                    lookup(
                        &successor,
                        &other,
                        &mut DiskRead::default(),
                        FactsNeed::Full
                    )
                    .unwrap(),
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
    fn an_inherited_full_copy_is_kept_over_fresh_facts() {
        for fresh in [pushed_facts(5, 5), index_facts(5, 5)] {
            let namespace = isolated_namespace();
            let built = pushed_facts(5, 5);
            let entry = revision(1);
            let left = left_behind(&namespace, &entry, &built);
            let stored = fs::metadata(&left).unwrap().len() as usize;
            let second = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(budget(&second), (1, 0, stored, 0));
            assert!(!second.has_entry(&entry).unwrap());
            let expected = if fresh.is_full() {
                Inserted::AlreadyFull
            } else {
                Inserted::Listed
            };
            assert_eq!(
                second.insert(&entry, &fresh, |_| true, |_| true).unwrap(),
                expected
            );
            let stats = second.stats();
            assert_eq!((stats.entries, stats.retired, stats.writes), (1, 0, 0));
            assert_eq!(budget(&second), (1, 0, stored, 0));
            assert!(!second.has_entry(&entry).unwrap());
            assert!(left.exists());
            assert!(matches!(
                lookup(&second, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
                DiskLookup::Hit(_)
            ));
            assert!(second.has_entry(&entry).unwrap());
            assert!(!left.exists());
            assert_eq!(budget(&second), (1, stored, 0, 0));
            drop(second);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn fresh_facts_replace_an_unverified_inherited_unbuilt_copy() {
        let namespace = isolated_namespace();
        let compact = index_facts(5, 5);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &compact);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&second), (1, 0, stored, 0));
        assert_eq!(
            second.insert(&entry, &compact, |_| true, |_| true).unwrap(),
            Inserted::Written
        );
        let stats = second.stats();
        assert_eq!((stats.entries, stats.retired, stats.writes), (1, 1, 1));
        assert_eq!(budget(&second), (1, stored, 0, 0));
        assert!(second.has_entry(&entry).unwrap());
        assert!(!left.exists());
        assert_eq!(text_state_of(&second, &entry), Some(TextState::Unbuilt));
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn live_owner_is_never_claimed() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let live = reuse_cache(&namespace, 64 * 1024);
        assert!(live
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let second = reuse_cache(&namespace, 64 * 1024);
        assert!(second.state.lock().unwrap().claimed.is_empty());
        assert_eq!(budget(&second), (0, 0, 0, 0));
        assert_eq!(second.decoded_bytes(&entry, FactsNeed::Full), None);
        assert!(matches!(
            lookup(&live, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
        let size = HEADER_BYTES + encoded_sections_len(&built);
        let cap = 2 * size;
        let first = reuse_cache(&namespace, cap);
        for generation in 1..=2 {
            assert!(first
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap()
                .serves());
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
                index_bytes: 0,
                index_accounted: 0,
                text_state: 1,
                scope: FactsNeed::Full,
            }
            .encode(),
            size,
        );
        let second = reuse_cache(&namespace, cap);
        assert_eq!(budget(&second), (2, 0, cap, size));
        assert!(second
            .insert(&revision(3), &built, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(budget(&second), (2, size, size, 0));
        let stats = second.stats();
        assert_eq!((stats.retired, stats.writes), (1, 1));
        assert!(!garbage.exists());
        assert!(!dead.exists());
        assert!(second.has_entry(&revision(3)).unwrap());
        let kept = (1..=2)
            .filter(|generation| {
                second
                    .decoded_bytes(&revision(*generation), FactsNeed::Full)
                    .is_some()
            })
            .count();
        assert_eq!(kept, 1);
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn only_the_three_newest_generations_are_listed() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let size = HEADER_BYTES + encoded_sections_len(&built);
        for generation in 1..=4 {
            let owner = reuse_cache(&namespace, 64 * 1024);
            assert!(owner
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap()
                .serves());
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
        assert_eq!(fifth.decoded_bytes(&revision(1), FactsNeed::Full), None);
        for generation in 2..=4 {
            assert!(
                fifth
                    .decoded_bytes(&revision(generation), FactsNeed::Full)
                    .is_some(),
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
        assert!(first
            .insert(&listed, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let dead = first.dir.clone();
        let left = first.entry_file(&listed);
        let stored = first.stats().bytes;
        let (index, _) = sections(&first, &listed);
        drop(first);
        let foreign = planted(
            &dead,
            &EntryName {
                key: [7; 32],
                tag: !engine_tag(&origin(Some("producer-a"))),
                bytes: 100,
                accounted: 0,
                index_bytes: 0,
                index_accounted: 0,
                text_state: 1,
                scope: FactsNeed::Full,
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
                index_bytes: index,
                index_accounted: built.index_accounted_bytes(),
                text_state: 1,
                scope: FactsNeed::Full,
            }
            .encode(),
            stored,
        );
        let temp = planted(&dead, c"ff.tmp", 10);
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&second), (1, 0, stored, 100 + stored));
        assert!(second
            .insert(&revision(2), &built, |_| true, |_| true)
            .unwrap()
            .serves());
        assert!(left.exists());
        assert!(!foreign.exists());
        assert!(!twin.exists());
        assert!(!temp.exists());
        assert_eq!(budget(&second), (2, stored, stored, 0));
        assert!(matches!(
            lookup(&second, &listed, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
            lookup(&third, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
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
                    index_bytes: 0,
                    index_accounted: 0,
                    text_state: 1,
                    scope: FactsNeed::Full,
                }
                .encode(),
                1024,
            );
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(
                successor.decoded_bytes(&entry, FactsNeed::Full),
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
            lookup(
                &successor,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (0, 0, 0, stored));
        assert!(left.is_dir());
        assert!(!successor.has_entry(&entry).unwrap());
        assert!(successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
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
        let size = HEADER_BYTES + encoded_sections_len(&built);
        let first = reuse_cache(&namespace, 2 * size);
        for generation in 1..=2 {
            assert!(first
                .insert(&revision(generation), &built, |_| true, |_| true)
                .unwrap()
                .serves());
        }
        let dead = first.dir.clone();
        let files = [
            first.entry_file(&revision(1)),
            first.entry_file(&revision(2)),
        ];
        drop(first);
        let successor = reuse_cache(&namespace, size);
        assert_eq!(budget(&successor), (1, 0, size, size));
        let (adopted, dropped): (usize, usize) = if successor
            .decoded_bytes(&revision(1), FactsNeed::Full)
            .is_some()
        {
            (0, 1)
        } else {
            (1, 0)
        };
        let entry = revision(adopted as u64 + 1);
        assert!(successor.decoded_bytes(&entry, FactsNeed::Full).is_some());
        assert!(matches!(
            lookup(
                &successor,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
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
                    index_bytes: 0,
                    index_accounted: 0,
                    text_state: 1,
                    scope: FactsNeed::Full,
                }
                .encode(),
                100,
            );
        }
        let successor = reuse_cache(&namespace, 1 << 20);
        assert_eq!(budget(&successor), (1, 0, stored, 300 * 100));
        successor.state.lock().unwrap().claimed[0].listed = false;
        let entry = revision(2);
        assert_eq!(
            successor
                .insert(&entry, &built, |_| true, |_| true)
                .unwrap(),
            Inserted::Refused
        );
        assert_eq!(successor.stats().writes, 0);
        assert!(!successor.has_entry(&entry).unwrap());
        assert!(!successor.state.lock().unwrap().claimed[0].listed);
        assert!(successor
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
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
                    index_bytes: 0,
                    index_accounted: 0,
                    text_state: 1,
                    scope: FactsNeed::Full,
                }
                .encode(),
                1024,
            );
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(
                successor.decoded_bytes(&entry, FactsNeed::Full).is_some(),
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
        let larger = HEADER_BYTES + encoded_sections_len(&large);
        assert!(larger > stored, "{larger} against {stored}");
        let successor = reuse_cache(&namespace, stored + larger - 1);
        assert_eq!(budget(&successor), (1, 0, stored, 0));
        fs::remove_file(&left).unwrap();
        fs::create_dir(&left).unwrap();
        assert!(matches!(
            lookup(
                &successor,
                &revision(1),
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (0, 0, 0, stored));
        let kept = revision(2);
        assert!(successor
            .insert(&kept, &small, |_| true, |_| true)
            .unwrap()
            .serves());
        let before = successor.stats();
        assert_eq!(
            successor
                .insert(&revision(3), &large, |_| true, |_| true)
                .unwrap(),
            Inserted::Refused
        );
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
        assert_eq!(
            successor
                .insert(&entry, &built, |_| true, |_| true)
                .unwrap(),
            Inserted::Refused
        );
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
                .unwrap()
                .serves());
        }
        let waiting = first.entry_file(&revision(2));
        let stored = fs::metadata(&waiting).unwrap().len() as usize;
        drop(first);
        let successor = reuse_cache(&namespace, 1 << 20);
        assert_eq!(budget(&successor), (48, 0, 48 * stored, 0));
        assert!(successor
            .insert(&revision(49), &built, |_| true, |_| true)
            .unwrap()
            .serves());
        assert!(successor.state.lock().unwrap().claimed[0].cursor.is_some());
        assert!(!successor.state.lock().unwrap().claimed[0].dirty);
        assert!(matches!(
            lookup(
                &successor,
                &revision(1),
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
            DiskLookup::Hit(_)
        ));
        assert!(successor.state.lock().unwrap().claimed[0].dirty);
        let mut inserts = 0u64;
        while !successor.state.lock().unwrap().claimed[0].swept {
            assert!(inserts < 64, "the restarted pass never settled");
            assert!(successor
                .insert(&revision(50 + inserts), &built, |_| true, |_| true)
                .unwrap()
                .serves());
            inserts += 1;
        }
        assert_eq!(budget(&successor).3, 0);
        assert!(successor
            .decoded_bytes(&revision(2), FactsNeed::Full)
            .is_some());
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
            .unwrap()
            .serves());
        let settled = {
            let state = successor.state.lock().unwrap();
            let owner = &state.claimed[0];
            (owner.swept, owner.listed, owner.garbage)
        };
        assert_eq!(settled, (true, true, 0));
        assert!(matches!(
            lookup(&successor, &lost, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Miss
        ));
        assert_eq!(budget(&successor), (1, stored, 0, 0));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn a_cache_without_a_producer_names_entries_by_state_and_shares_nothing_across_restarts() {
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let entry = revision(1);
        let plain = cache(64 * 1024);
        assert!(plain
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let unbuilt = plain.entry_file(&entry);
        let unbuilt_name = file_name(&unbuilt);
        let unbuilt_size = fs::metadata(&unbuilt).unwrap().len() as usize;
        assert_eq!(unbuilt_name.to_bytes().len(), ENTRY_NAME_BYTES);
        let parsed = EntryName::parse(&unbuilt_name).expect("a state-bearing name");
        assert_eq!(
            (parsed.key, parsed.tag, parsed.bytes, parsed.text_state),
            (entry.name, tag(&plain.owner_compatibility), unbuilt_size, 0)
        );
        assert!(plain
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        let present = plain.entry_file(&entry);
        let present_size = fs::metadata(&present).unwrap().len() as usize;
        assert_ne!(present, unbuilt);
        assert_eq!(
            EntryName::parse(&file_name(&present)).unwrap().text_state,
            1
        );
        assert!(!unbuilt.exists());
        assert_eq!(budget(&plain), (1, present_size, 0, 0));
        assert_eq!(entry_files(&plain), [present.clone()]);
        assert_eq!(physical_bytes(&plain.dir), present_size);
        assert!(matches!(
            lookup(&plain, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Hit(_)
        ));
        let namespace = plain.namespace.clone();
        let dir = plain.dir.clone();
        drop(plain);
        assert!(!dir.exists(), "the dropped cache left its directory behind");
        let next = cache(64 * 1024);
        assert_eq!(next.namespace, namespace);
        assert_ne!(next.dir, dir);
        assert_eq!(next.decoded_bytes(&entry, FactsNeed::Index), None);
        assert_eq!(budget(&next), (0, 0, 0, 0));
        assert!(next.state.lock().unwrap().claimed.is_empty());
    }

    #[test]
    fn a_crashed_cache_without_a_producer_is_cleaned_by_its_successor_under_the_new_names() {
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
        let stale = file_path.with_file_name(
            EntryName {
                key: revision(7).name,
                tag: 1,
                bytes: 50,
                accounted: 0,
                index_bytes: 0,
                index_accounted: 0,
                text_state: 1,
                scope: FactsNeed::Full,
            }
            .encode()
            .to_str()
            .unwrap(),
        );
        fs::rename(&file_path, &stale).unwrap();
        assert_eq!(file_name(&stale).to_bytes().len(), ENTRY_NAME_BYTES);
        for remaining in [34, 18, 2] {
            cleaner.cleanup_stale_owners_with_limits(128, 16);
            assert_eq!(fs::metadata(&stale).unwrap().len(), remaining);
            assert!(dir.exists());
        }
        cleaner.cleanup_stale_owners_with_limits(128, 16);
        assert!(!dir.exists());
        drop(cleaner);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn entry_name_round_trips() {
        let name = EntryName {
            key: std::array::from_fn(|index| index as u8),
            tag: 0x0123_4567_89ab_cdef,
            bytes: 4096,
            accounted: 2048,
            index_bytes: 512,
            index_accounted: 256,
            text_state: TextState::Present as u64,
            scope: FactsNeed::Full,
        };
        let encoded = name.encode();
        let text = encoded.to_str().unwrap();
        assert_eq!(
            text,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
             0123456789abcdef000000000000100000000000000008000000000000000200\
             00000000000001000000000000000001.facts"
        );
        assert_eq!(text.len(), ENTRY_NAME_BYTES);
        assert!(!generated_entry(&encoded));
        assert_eq!(legacy_entry_bytes(&encoded), None);
        let parsed = EntryName::parse(&encoded).unwrap();
        assert_eq!(
            (
                parsed.key,
                parsed.tag,
                parsed.bytes,
                parsed.accounted,
                parsed.index_bytes,
                parsed.index_accounted,
                parsed.text_state
            ),
            (
                name.key,
                name.tag,
                name.bytes,
                name.accounted,
                name.index_bytes,
                name.index_accounted,
                name.text_state
            )
        );
        for (legacy_bytes, hex_len) in LEGACY_ENTRY_NAME_BYTES.iter().zip([144, 112]) {
            let legacy = CString::new(format!("{}.facts", &text[..hex_len])).unwrap();
            assert_eq!(legacy.to_bytes().len(), *legacy_bytes);
            assert!(EntryName::parse(&legacy).is_none());
            assert!(!generated_entry(&legacy));
            assert_eq!(legacy_entry_bytes(&legacy), Some(4096));
        }
        for (case, rejected) in [
            ("one byte longer", format!("0{text}")),
            ("one byte shorter", text[1..].to_owned()),
            ("the wrong suffix", format!("{}.fact0", &text[..160])),
            ("an uppercase body", text.replacen('a', "A", 1)),
            ("a non-hex body", format!("g{}", &text[1..])),
        ] {
            assert!(
                EntryName::parse(&CString::new(rejected).unwrap()).is_none(),
                "{case}"
            );
        }
        let unknown = EntryName {
            text_state: 3,
            ..name
        }
        .encode();
        assert_eq!(EntryName::parse(&unknown).unwrap().text_state, 3);
        assert_eq!(TextState::from_raw(3), None);
    }

    #[test]
    fn index_lookup_reads_only_the_index_section() {
        let cache = cache(4 * 1024 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let mut built = PreparedFacts {
            evidence: crate::snapshot_prepared::PreparedEvidence::Index {
                inputs: json!({
                    "calls": [["Read", ["/repo/file.rs"]]],
                    "commands": [],
                    "edited_files": [],
                    "skills": [],
                }),
                has_error: true,
            },
            overrides: OverrideFacts::Loaded(vec![OverrideEvent {
                text: "x".repeat(1024 * 1024),
                tools: vec!["Read".to_owned()],
            }]),
            accounted: 0,
        };
        built.refresh_accounted();
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let stored = cache.entry_file_len(&entry);
        let (index, text) = sections(&cache, &entry);
        assert!(
            text >= 1024 * 1024 && index < 1024,
            "{index} index bytes, {text} text bytes"
        );
        assert_eq!(
            cache.read_bound(&entry, FactsNeed::Index),
            Some(HEADER_BYTES + index)
        );
        assert_eq!(
            cache.read_bound(&entry, FactsNeed::Full),
            Some(stored as usize + 1)
        );
        let mut read = DiskRead::default();
        let DiskLookup::Hit(decoded) = lookup(&cache, &entry, &mut read, FactsNeed::Index).unwrap()
        else {
            panic!("expected index facts");
        };
        assert_eq!(
            (read.operations, read.bytes as usize),
            (1, HEADER_BYTES + index)
        );
        assert!(index_only(&decoded));
        assert_eq!(decoded.index_inputs(), built.index_inputs());
        assert!(decoded.has_error());
        assert!(decoded.accounted_bytes() <= built.index_accounted_bytes());
        assert!(decoded.serves(FactsNeed::Index) && !decoded.serves(FactsNeed::Full));
        let mut full = DiskRead::default();
        let DiskLookup::Hit(loaded) = lookup(&cache, &entry, &mut full, FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert_eq!((full.operations, full.bytes), (1, stored));
        assert!(loaded.is_full());
        assert_eq!(
            loaded.text_section().unwrap()[0].text,
            "x".repeat(1024 * 1024)
        );
        assert_eq!(loaded.text_section().unwrap()[0].tools, ["Read"]);
        assert!(loaded.accounted_bytes() <= built.accounted_bytes());
        assert!(cache
            .touch(&entry, FactsNeed::Index, |accounted| accounted
                == built.index_accounted_bytes())
            .unwrap());
        assert!(cache
            .touch(&entry, FactsNeed::Full, |accounted| accounted
                == built.accounted_bytes())
            .unwrap());
    }

    #[test]
    fn exceeded_override_text_round_trips_through_both_needs() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let mut built = pushed_facts(1, 1);
        built.overrides = OverrideFacts::Exceeded;
        built.refresh_accounted();
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let DiskLookup::Hit(full) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert!(matches!(full.overrides, OverrideFacts::Exceeded));
        assert!(full.is_full());
        let refused = full
            .query(&json!({"kind":"has_override","token":"t","invalidated_by":[]}))
            .err()
            .unwrap();
        assert_eq!(
            (refused.status, refused.reason.as_str()),
            (
                crate::snapshot::Status::Incomplete,
                "prepared override text exceeded its bound"
            )
        );
        let DiskLookup::Hit(index) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Index).unwrap()
        else {
            panic!("expected index facts");
        };
        assert!(index_only(&index));
        assert_eq!(index.accounted_bytes(), full.accounted_bytes());
    }

    #[test]
    fn a_corrupt_text_section_retires_only_the_full_lookup() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let built = pushed_facts(3, 2);
        assert!(cache
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let (index, _) = sections(&cache, &entry);
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, &bytes).unwrap();
        let mut read = DiskRead::default();
        assert!(matches!(
            lookup(&cache, &entry, &mut read, FactsNeed::Index).unwrap(),
            DiskLookup::Hit(decoded) if index_only(&decoded) && decoded.index_inputs() == built.index_inputs()
        ));
        assert_eq!(read.bytes as usize, HEADER_BYTES + index);
        assert_eq!(cache.stats().entries, 1);
        let mut full = DiskRead::default();
        assert!(matches!(
            lookup(&cache, &entry, &mut full, FactsNeed::Full).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(full.bytes, bytes.len() as u64);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn a_corrupt_index_section_retires_both_lookups() {
        let cache = cache(64 * 1024);
        let built = pushed_facts(3, 2);
        for need in [FactsNeed::Index, FactsNeed::Full] {
            let entry = key(stamp(1), &json!({"root":"/repo"}));
            assert!(cache
                .insert(&entry, &built, |_| true, |_| true)
                .unwrap()
                .serves());
            let path = cache.entry_path(entry.digest);
            let mut bytes = fs::read(&path).unwrap();
            bytes[HEADER_BYTES + 4] ^= 1;
            fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                lookup(&cache, &entry, &mut DiskRead::default(), need).unwrap(),
                DiskLookup::Retired
            ));
            assert_eq!(cache.stats().entries, 0);
        }
    }

    #[test]
    fn a_header_whose_lengths_disagree_with_the_name_is_retired_before_decoding() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert!(cache
            .insert(&entry, &pushed_facts(2, 2), |_| true, |_| true)
            .unwrap()
            .serves());
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        bytes[40] ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Index).unwrap(),
            DiskLookup::Retired
        ));
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn successor_reads_only_the_index_of_an_inherited_entry() {
        let namespace = isolated_namespace();
        let built = pushed_facts(5, 5);
        let entry = revision(1);
        let first = reuse_cache(&namespace, 64 * 1024);
        assert!(first
            .insert(&entry, &built, |_| true, |_| true)
            .unwrap()
            .serves());
        let (index, _) = sections(&first, &entry);
        let stored = first.entry_file_len(&entry);
        drop(first);
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(
            second.read_bound(&entry, FactsNeed::Index),
            Some(HEADER_BYTES + index)
        );
        assert_eq!(
            second.decoded_bytes(&entry, FactsNeed::Index),
            Some(
                HEADER_BYTES
                    + index
                    + built.index_accounted_bytes()
                    + decode_transients_walk(index)
            )
        );
        let mut read = DiskRead::default();
        let DiskLookup::Hit(loaded) = lookup(&second, &entry, &mut read, FactsNeed::Index).unwrap()
        else {
            panic!("expected inherited index facts");
        };
        assert!(index_only(&loaded));
        assert_eq!(loaded.index_inputs(), built.index_inputs());
        assert_eq!(
            (read.operations, read.bytes as usize),
            (1, HEADER_BYTES + index)
        );
        assert!(second.has_entry(&entry).unwrap());
        assert_eq!(second.stats().bytes, stored as usize);
        drop(second);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn legacy_entries_are_swept_as_garbage_and_never_adopted() {
        let namespace = isolated_namespace();
        let built = pushed_facts(1, 1);
        let kept = revision(1);
        let left = left_behind(&namespace, &kept, &built);
        let stored = fs::metadata(&left).unwrap().len() as usize;
        let dir = left.parent().unwrap().to_path_buf();
        let legacy_names = [
            CString::new(format!(
                "{}{:016x}{:016x}{:016x}{:016x}{:016x}.facts",
                "ab".repeat(32),
                0u64,
                4096usize,
                2048usize,
                512usize,
                256usize
            ))
            .unwrap(),
            CString::new(format!(
                "{}{:016x}{:016x}{:016x}.facts",
                "cd".repeat(32),
                0u64,
                4096usize,
                2048usize
            ))
            .unwrap(),
        ];
        for (legacy, bytes) in legacy_names.iter().zip(LEGACY_ENTRY_NAME_BYTES) {
            assert_eq!(legacy.to_bytes().len(), bytes);
            assert_eq!(legacy_entry_bytes(legacy), Some(4096));
        }
        let planted_paths: Vec<PathBuf> = legacy_names
            .iter()
            .map(|legacy| planted(&dir, legacy, 4096))
            .collect();
        let successor = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(budget(&successor), (1, 0, stored, 2 * 4096));
        assert!(successor.decoded_bytes(&kept, FactsNeed::Index).is_some());
        for _ in 0..4 {
            assert!(successor
                .insert(&revision(2), &built, |_| true, |_| true)
                .unwrap()
                .serves());
        }
        for planted_path in &planted_paths {
            assert!(
                !planted_path.exists(),
                "the legacy entry {planted_path:?} survived the owner sweep"
            );
        }
        assert!(
            left.exists(),
            "the valid entry was swept with the legacy names"
        );
        assert_eq!(budget(&successor), (2, stored, stored, 0));
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    fn index_facts(events: usize, tools: usize) -> PreparedFacts {
        pushed_facts(events, tools).without_text()
    }

    fn encoded_index_len(facts: &PreparedFacts) -> usize {
        sonic_rs::to_vec(&facts.index_section()).unwrap().len()
    }

    fn text_state_of(cache: &PreparedDiskCache, entry: &PreparedDiskKey) -> Option<TextState> {
        cache
            .state
            .lock()
            .unwrap()
            .entries
            .get(&entry.name)
            .map(|listed| listed.text_state)
    }

    fn stranded(cache: &PreparedDiskCache) -> Vec<(CString, usize)> {
        cache.state.lock().unwrap().stranded_owned.clone()
    }

    fn entry_files(cache: &PreparedDiskCache) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&cache.dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|listed| listed.path())
            .filter(|path| path.file_name().is_some_and(|name| name != "owner.lock"))
            .collect();
        files.sort();
        files
    }

    fn replace_with_directory(path: &Path) {
        fs::remove_file(path).unwrap();
        fs::create_dir(path).unwrap();
    }

    fn planted_bytes(dir: &Path, name: &CStr, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name.to_str().unwrap());
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        path
    }

    fn file_name(path: &Path) -> CString {
        CString::new(path.file_name().unwrap().to_str().unwrap()).unwrap()
    }

    fn strand_old_name(cache: &PreparedDiskCache, dir: PathBuf) {
        cache.set_upgrade_hook(Some(Box::new(
            move |step: UpgradeStep, _: &CStr, old: &CStr| {
                if step == UpgradeStep::Published {
                    replace_with_directory(&dir.join(old.to_str().unwrap()));
                }
            },
        )));
    }

    #[test]
    fn index_only_facts_write_only_the_index_and_upgrade_once() {
        index_only_facts_upgrade_once(&cache(64 * 1024));
        let namespace = isolated_namespace();
        let reusing = reuse_cache(&namespace, 64 * 1024);
        index_only_facts_upgrade_once(&reusing);
        drop(reusing);
        fs::remove_dir_all(namespace).unwrap();
    }

    fn index_only_facts_upgrade_once(cache: &PreparedDiskCache) {
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let stored = cache.entry_file_len(&entry) as usize;
        assert_eq!(stored, HEADER_BYTES + encoded_index_len(&compact));
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Unbuilt));
        assert_eq!(cache.read_bound(&entry, FactsNeed::Index), Some(stored));
        assert_eq!(cache.read_bound(&entry, FactsNeed::Full), Some(stored + 1));
        assert_eq!(
            cache.decoded_bytes(&entry, FactsNeed::Index),
            Some(
                stored
                    + compact.index_accounted_bytes()
                    + decode_transients_walk(stored - HEADER_BYTES)
            )
        );
        assert_eq!(
            cache.decoded_bytes(&entry, FactsNeed::Full),
            cache.decoded_bytes(&entry, FactsNeed::Index)
        );
        let mut read = DiskRead::default();
        let DiskLookup::Hit(decoded) = lookup(&cache, &entry, &mut read, FactsNeed::Index).unwrap()
        else {
            panic!("expected index facts");
        };
        assert!(index_only(&decoded) && decoded.index_inputs() == compact.index_inputs());
        assert_eq!((read.operations, read.bytes as usize), (1, stored));
        let mut probed = DiskRead::default();
        let DiskLookup::IndexOnly(partial) =
            lookup(&cache, &entry, &mut probed, FactsNeed::Full).unwrap()
        else {
            panic!("expected an index-only answer to the full lookup");
        };
        assert!(index_only(&partial) && partial.index_inputs() == compact.index_inputs());
        assert_eq!((probed.operations, probed.bytes as usize), (1, stored));
        assert!(cache
            .touch(&entry, FactsNeed::Full, |accounted| accounted
                == compact.index_accounted_bytes())
            .unwrap());
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!((cache.stats().writes, cache.stats().entries), (1, 1));
        assert!(cache
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        let upgraded = cache.entry_file_len(&entry) as usize;
        assert_eq!(upgraded, HEADER_BYTES + encoded_sections_len(&full));
        assert!(upgraded > stored);
        let stats = cache.stats();
        assert_eq!(
            (
                stats.entries,
                stats.bytes,
                stats.retired,
                stats.writes,
                stats.write_bytes
            ),
            (1, upgraded, 0, 2, (stored + upgraded) as u64)
        );
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Present));
        assert_eq!(entry_files(&cache), [cache.entry_file(&entry)]);
        let DiskLookup::Hit(loaded) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert!(loaded.is_full());
        assert_eq!(loaded.text_section().unwrap().len(), 3);
        assert_eq!(
            cache.insert(&entry, &compact, |_| true, |_| true).unwrap(),
            Inserted::Listed
        );
        assert_eq!(
            cache.insert(&entry, &full, |_| true, |_| true).unwrap(),
            Inserted::AlreadyFull
        );
        assert_eq!(cache.stats().writes, 2);
        assert_eq!(text_state_of(cache, &entry), Some(TextState::Present));
    }

    #[test]
    fn an_upgrade_records_exceeded_text_on_demand() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert!(cache
            .insert(&entry, &index_facts(2, 2), |_| true, |_| true)
            .unwrap()
            .serves());
        let mut exceeded = pushed_facts(2, 2);
        exceeded.overrides = OverrideFacts::Exceeded;
        exceeded.refresh_accounted();
        assert!(cache
            .insert(&entry, &exceeded, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Exceeded));
        assert_eq!(cache.stats().writes, 2);
        let DiskLookup::Hit(full) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert!(matches!(full.overrides, OverrideFacts::Exceeded));
        let DiskLookup::Hit(index) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Index).unwrap()
        else {
            panic!("expected index facts");
        };
        assert!(index_only(&index));
        assert!(cache
            .insert(&entry, &pushed_facts(2, 2), |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Exceeded));
        assert_eq!(cache.stats().writes, 2);
    }

    #[test]
    fn a_full_lookup_whose_admitted_bound_is_stale_is_refused_before_reading() {
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let compact = index_facts(3, 2);
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let admitted = cache.decoded_bytes(&entry, FactsNeed::Full).unwrap();
        assert!(cache
            .insert(&entry, &pushed_facts(3, 2), |_| true, |_| true)
            .unwrap()
            .serves());
        assert!(cache.decoded_bytes(&entry, FactsNeed::Full).unwrap() > admitted);
        let mut read = DiskRead::default();
        assert!(matches!(
            cache
                .lookup(
                    &entry,
                    &mut read,
                    FactsNeed::Full,
                    admitted,
                    &Cancellation::default(),
                    u64::MAX,
                )
                .unwrap(),
            DiskLookup::Changed
        ));
        assert_eq!((read.operations, read.bytes), (0, 0));
        let DiskLookup::Hit(full) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert!(full.is_full());
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Present));
    }

    #[test]
    fn a_corrupt_unbuilt_index_retires_both_needs() {
        let cache = cache(64 * 1024);
        for need in [FactsNeed::Index, FactsNeed::Full] {
            let entry = key(stamp(1), &json!({"root":"/repo"}));
            assert!(cache
                .insert(&entry, &index_facts(3, 2), |_| true, |_| true)
                .unwrap()
                .serves());
            let path = cache.entry_path(entry.digest);
            let mut bytes = fs::read(&path).unwrap();
            bytes[HEADER_BYTES + 4] ^= 1;
            fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                lookup(&cache, &entry, &mut DiskRead::default(), need).unwrap(),
                DiskLookup::Retired
            ));
            assert_eq!(cache.stats().entries, 0);
            assert!(!path.exists());
        }
    }

    #[test]
    fn a_header_whose_text_state_disagrees_with_the_name_is_retired_before_decoding() {
        let cache = cache(64 * 1024);
        for (case, facts, byte) in [
            ("an unknown state byte", index_facts(2, 2), 3u8),
            (
                "a present header under an unbuilt name",
                index_facts(2, 2),
                TextState::Present as u8,
            ),
            (
                "an unbuilt header under a present name",
                pushed_facts(2, 2),
                TextState::Unbuilt as u8,
            ),
        ] {
            let entry = key(stamp(1), &json!({"root":"/repo"}));
            assert!(cache
                .insert(&entry, &facts, |_| true, |_| true)
                .unwrap()
                .serves());
            let path = cache.entry_path(entry.digest);
            let mut bytes = fs::read(&path).unwrap();
            bytes[136] = byte;
            fs::write(&path, &bytes).unwrap();
            for need in [FactsNeed::Index, FactsNeed::Full] {
                assert!(
                    matches!(
                        lookup(&cache, &entry, &mut DiskRead::default(), need).unwrap(),
                        DiskLookup::Retired | DiskLookup::Miss
                    ),
                    "{case}"
                );
            }
            assert_eq!(cache.stats().entries, 0, "{case}");
        }
    }

    #[test]
    fn refused_upgrades_leave_the_unbuilt_entry_untouched() {
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let (old_size, new_size) = (
            HEADER_BYTES + encoded_index_len(&compact),
            HEADER_BYTES + encoded_sections_len(&full),
        );
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let snapshot = |cache: &PreparedDiskCache| {
            (
                budget(cache),
                cache.stats().writes,
                cache.entry_file_len(&entry),
                text_state_of(cache, &entry),
                entry_files(cache),
            )
        };
        let tight = cache(old_size + new_size - 1);
        assert!(tight
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let before = snapshot(&tight);
        assert_eq!(before.0, (1, old_size, 0, 0));
        assert_eq!(
            tight.insert(&entry, &full, |_| false, |_| true).unwrap(),
            Inserted::Refused
        );
        assert_eq!(
            snapshot(&tight),
            before,
            "a refused reservation moved the entry"
        );
        assert_eq!(
            tight.insert(&entry, &full, |_| true, |_| true).unwrap(),
            Inserted::Refused
        );
        assert_eq!(
            snapshot(&tight),
            before,
            "refused physical room moved the entry"
        );
        let roomy = cache(old_size + new_size);
        assert!(roomy
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let before = snapshot(&roomy);
        let dir = roomy.dir.clone();
        roomy.set_upgrade_hook(Some(Box::new(
            move |step: UpgradeStep, fresh: &CStr, _: &CStr| {
                if step == UpgradeStep::Staged {
                    fs::create_dir(dir.join(fresh.to_str().unwrap())).unwrap();
                }
            },
        )));
        let error = roomy
            .insert(&entry, &full, |_| true, |_| true)
            .err()
            .expect("the blocked rename landed");
        roomy.set_upgrade_hook(None);
        assert_eq!(error.status, crate::snapshot::Status::Incomplete);
        assert!(
            error.reason.starts_with("prepared facts disk cache: "),
            "{error:?}"
        );
        let blocking = entry_files(&roomy)
            .into_iter()
            .find(|path| path.is_dir())
            .expect("the blocking directory");
        fs::remove_dir(&blocking).unwrap();
        assert_eq!(snapshot(&roomy), before, "a failed rename moved the entry");
        assert!(roomy
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(budget(&roomy), (1, new_size, 0, 0));
        assert_eq!(text_state_of(&roomy, &entry), Some(TextState::Present));
        assert_eq!(entry_files(&roomy), [roomy.entry_file(&entry)]);
    }

    #[test]
    fn a_failed_rename_whose_temp_cannot_be_unlinked_strands_the_temp_until_a_sweep_clears_it() {
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let old_size = cache.entry_file_len(&entry) as usize;
        let new_size = HEADER_BYTES + encoded_sections_len(&full);
        let dir = cache.dir.clone();
        cache.set_upgrade_hook(Some(Box::new(
            move |step: UpgradeStep, fresh: &CStr, _: &CStr| {
                if step == UpgradeStep::Staged {
                    let temp = fs::read_dir(&dir)
                        .unwrap()
                        .filter_map(Result::ok)
                        .map(|listed| listed.path())
                        .find(|path| path.extension().is_some_and(|ext| ext == "tmp"))
                        .expect("the staged temp file");
                    replace_with_directory(&temp);
                    fs::write(dir.join(fresh.to_str().unwrap()), b"blocker").unwrap();
                }
            },
        )));
        let error = cache
            .insert(&entry, &full, |_| true, |_| true)
            .err()
            .expect("the rename of a directory over a file landed");
        cache.set_upgrade_hook(None);
        assert_eq!(error.status, crate::snapshot::Status::Incomplete);
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Unbuilt));
        assert_eq!(budget(&cache), (1, old_size + new_size, 0, new_size));
        let temp = stranded(&cache);
        assert_eq!(temp.len(), 1);
        assert_eq!(temp[0].1, new_size);
        let temp_dir = cache.dir.join(temp[0].0.to_str().unwrap());
        assert!(temp_dir.is_dir());
        let blocker = entry_files(&cache)
            .into_iter()
            .find(|path| path.is_file() && fs::metadata(path).unwrap().len() == 7)
            .expect("the blocking destination");
        fs::remove_file(blocker).unwrap();
        assert!(cache
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Present));
        let upgraded = cache.entry_file_len(&entry) as usize;
        assert_eq!(budget(&cache), (1, upgraded + new_size, 0, new_size));
        fs::remove_dir(&temp_dir).unwrap();
        assert!(cache
            .insert(
                &key(stamp(2), &json!({"root":"/repo"})),
                &compact,
                |_| true,
                |_| true
            )
            .unwrap()
            .serves());
        assert!(stranded(&cache).is_empty());
        assert_eq!(budget(&cache), (2, upgraded + old_size, 0, 0));
        assert_eq!(physical_bytes(&cache.dir), upgraded + old_size);
    }

    fn physical_bytes(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|listed| listed.file_name() != "owner.lock")
            .filter_map(|listed| listed.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len() as usize)
            .sum()
    }

    #[test]
    fn an_owned_unlink_failure_strands_the_old_name_until_a_sweep_clears_it() {
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let old_path = cache.entry_file(&entry);
        let old_size = fs::metadata(&old_path).unwrap().len() as usize;
        strand_old_name(&cache, cache.dir.clone());
        assert!(cache
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        cache.set_upgrade_hook(None);
        let new_size = cache.entry_file_len(&entry) as usize;
        assert_eq!(text_state_of(&cache, &entry), Some(TextState::Present));
        assert_eq!(budget(&cache), (1, old_size + new_size, 0, old_size));
        assert_eq!(stranded(&cache), [(file_name(&old_path), old_size)]);
        assert!(old_path.is_dir());
        assert!(cache
            .insert(
                &key(stamp(2), &json!({"root":"/repo"})),
                &compact,
                |_| true,
                |_| true
            )
            .unwrap()
            .serves());
        assert_eq!(budget(&cache), (2, 2 * old_size + new_size, 0, old_size));
        assert_eq!(stranded(&cache).len(), 1);
        fs::remove_dir(&old_path).unwrap();
        assert!(cache
            .insert(
                &key(stamp(3), &json!({"root":"/repo"})),
                &compact,
                |_| true,
                |_| true
            )
            .unwrap()
            .serves());
        assert!(stranded(&cache).is_empty());
        assert_eq!(budget(&cache), (3, 2 * old_size + new_size, 0, 0));
        assert_eq!(entry_files(&cache).len(), 3);
        assert_eq!(physical_bytes(&cache.dir), 2 * old_size + new_size);
    }

    #[test]
    fn a_full_stranded_list_refuses_the_next_owned_upgrade_until_a_sweep_clears_it() {
        let cache = cache(8 * 1024 * 1024);
        strand_old_name(&cache, cache.dir.clone());
        let entries: Vec<PreparedDiskKey> = (1..=MAX_STRANDED_OWNED as u64)
            .map(|size| key(stamp(size), &json!({"root":"/repo"})))
            .collect();
        for entry in &entries {
            assert!(cache
                .insert(entry, &index_facts(1, 1), |_| true, |_| true)
                .unwrap()
                .serves());
        }
        let compact_size = cache.entry_file_len(&entries[0]) as usize;
        let strands = MAX_STRANDED_OWNED - 1;
        for entry in &entries[..strands] {
            assert!(cache
                .insert(entry, &pushed_facts(1, 1), |_| true, |_| true)
                .unwrap()
                .serves());
        }
        cache.set_upgrade_hook(None);
        assert_eq!(stranded(&cache).len(), strands);
        let full_size = cache.entry_file_len(&entries[0]) as usize;
        let last = &entries[strands];
        let before = budget(&cache);
        assert_eq!(
            before,
            (
                entries.len(),
                strands * (compact_size + full_size) + compact_size,
                0,
                strands * compact_size
            )
        );
        let writes = cache.stats().writes;
        assert_eq!(
            cache
                .insert(last, &pushed_facts(1, 1), |_| true, |_| true)
                .unwrap(),
            Inserted::Refused
        );
        assert_eq!(text_state_of(&cache, last), Some(TextState::Unbuilt));
        assert_eq!(cache.entry_file_len(last) as usize, compact_size);
        assert_eq!((budget(&cache), cache.stats().writes), (before, writes));
        assert_eq!(stranded(&cache).len(), strands);
        for path in entry_files(&cache).into_iter().filter(|path| path.is_dir()) {
            fs::remove_dir(path).unwrap();
        }
        assert!(cache
            .insert(last, &pushed_facts(1, 1), |_| true, |_| true)
            .unwrap()
            .serves());
        assert_eq!(text_state_of(&cache, last), Some(TextState::Present));
        assert!(stranded(&cache).is_empty());
        assert_eq!(
            budget(&cache),
            (entries.len(), entries.len() * full_size, 0, 0)
        );
        assert_eq!(physical_bytes(&cache.dir), entries.len() * full_size);
    }

    #[test]
    fn a_stranded_destination_is_never_published_over() {
        let compact = index_facts(3, 2);
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let destination = cache.entry_file(&entry);
        let size = fs::metadata(&destination).unwrap().len() as usize;
        {
            let mut state = cache.state.lock().unwrap();
            state.entries.remove(&entry.name).unwrap();
            state.stranded_owned.push((file_name(&destination), size));
        }
        replace_with_directory(&destination);
        assert_eq!(budget(&cache), (0, size, 0, size));
        let writes = cache.stats().writes;
        assert_eq!(
            cache.insert(&entry, &compact, |_| true, |_| true).unwrap(),
            Inserted::Refused
        );
        assert_eq!(
            (budget(&cache), cache.stats().writes),
            ((0, size, 0, size), writes)
        );
        assert_eq!(entry_files(&cache), [destination.clone()]);
        assert!(destination.is_dir());
        fs::remove_dir(&destination).unwrap();
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        assert!(stranded(&cache).is_empty());
        assert_eq!(budget(&cache), (1, size, 0, 0));
        assert_eq!(entry_files(&cache), [destination.clone()]);
        assert!(destination.is_file());
        assert_eq!(physical_bytes(&cache.dir), size);
    }

    #[test]
    fn a_temp_that_was_never_created_is_never_charged() {
        let compact = index_facts(3, 2);
        let cache = cache(64 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let next_temp = format!("{:x}.tmp", cache.state.lock().unwrap().clock + 1);
        let planted = cache.dir.join(&next_temp);
        fs::write(&planted, b"").unwrap();
        let error = cache
            .insert(&entry, &compact, |_| true, |_| true)
            .err()
            .expect("the staging open over an existing temp landed");
        assert_eq!(error.status, crate::snapshot::Status::Incomplete);
        assert_eq!(budget(&cache), (0, 0, 0, 0));
        assert!(stranded(&cache).is_empty());
        assert_eq!(cache.stats().writes, 0);
        fs::remove_file(&planted).unwrap();
        assert!(cache
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let size = cache.entry_file_len(&entry) as usize;
        assert_eq!(budget(&cache), (1, size, 0, 0));
        assert_eq!(physical_bytes(&cache.dir), size);
    }

    #[test]
    fn an_incomplete_listing_adopts_nothing() {
        let namespace = isolated_namespace();
        let full = pushed_facts(3, 2);
        let entry = revision(1);
        let left = left_behind(&namespace, &entry, &full);
        let dead = left.parent().unwrap().to_path_buf();
        for index in 0..MAX_LISTED_NAMES {
            fs::write(dead.join(format!("g{index:05}")), b"").unwrap();
        }
        let successor = reuse_cache(&namespace, 64 * 1024);
        assert!(successor.state.lock().unwrap().unread());
        assert_eq!(successor.decoded_bytes(&entry, FactsNeed::Index), None);
        assert_eq!(successor.decoded_bytes(&entry, FactsNeed::Full), None);
        let (entries, _, inherited, _) = budget(&successor);
        assert_eq!((entries, inherited), (0, 0));
        assert!(left.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn an_incomplete_listing_withdraws_every_provisional_entry_across_directories() {
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let entry = revision(1);
        let unbuilt_source = isolated_namespace();
        let writer = reuse_cache(&unbuilt_source, 64 * 1024);
        assert!(writer
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let unbuilt_path = writer.entry_file(&entry);
        let (unbuilt_name, unbuilt_bytes) =
            (file_name(&unbuilt_path), fs::read(&unbuilt_path).unwrap());
        drop(writer);
        fs::remove_dir_all(unbuilt_source).unwrap();
        let namespace = isolated_namespace();
        let full_path = left_behind(&namespace, &entry, &full);
        let older = full_path.parent().unwrap().to_path_buf();
        for index in 0..MAX_LISTED_NAMES {
            fs::write(older.join(format!("g{index:05}")), b"").unwrap();
        }
        let newer_owner = PreparedDiskCache::open(
            namespace.clone(),
            &epoch(),
            64 * 1024,
            &origin(Some("producer-a")),
            0,
            Work::default(),
        )
        .unwrap();
        let newer = newer_owner.dir.clone();
        let planted = planted_bytes(&newer, &unbuilt_name, &unbuilt_bytes);
        drop(newer_owner);
        let successor = reuse_cache(&namespace, 64 * 1024);
        {
            let state = successor.state.lock().unwrap();
            assert!(state.unread());
            assert_eq!(state.claimed.len(), 2);
            assert!(state.claimed[1].listed, "the newer listing was complete");
            assert!(!state.claimed[0].listed, "the older listing was incomplete");
        }
        let (entries, bytes, inherited, garbage) = budget(&successor);
        assert_eq!((entries, bytes, inherited), (0, 0, 0));
        assert!(garbage >= unbuilt_bytes.len());
        assert_eq!(successor.decoded_bytes(&entry, FactsNeed::Index), None);
        assert!(matches!(
            lookup(
                &successor,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Index
            )
            .unwrap(),
            DiskLookup::Miss
        ));
        assert!(matches!(
            lookup(
                &successor,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Full
            )
            .unwrap(),
            DiskLookup::Miss
        ));
        assert!(planted.exists());
        assert!(full_path.exists(), "the unseen full name was deleted");
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn restart_never_substitutes_an_unbuilt_name_for_a_refused_full_winner() {
        let namespace = isolated_namespace();
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let entry = revision(1);
        let first = reuse_cache(&namespace, 64 * 1024);
        assert!(first
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let unbuilt_path = first.entry_file(&entry);
        let unbuilt_bytes = fs::read(&unbuilt_path).unwrap();
        let unbuilt_decoded = first.decoded_bytes(&entry, FactsNeed::Index).unwrap();
        assert!(first
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        let full_bytes = fs::read(first.entry_file(&entry)).unwrap();
        assert!(first.decoded_bytes(&entry, FactsNeed::Full).unwrap() > unbuilt_decoded);
        planted_bytes(&first.dir, &file_name(&unbuilt_path), &unbuilt_bytes);
        let dead = first.dir.clone();
        drop(first);
        let index_growth = cache(64 * 1024)
            .state
            .lock()
            .unwrap()
            .entries
            .growth_for(&entry.name);
        let successor = PreparedDiskCache::open(
            namespace.clone(),
            &epoch(),
            64 * 1024,
            &origin(Some("producer-a")),
            index_growth + unbuilt_decoded,
            Work::default(),
        )
        .unwrap();
        assert_eq!(successor.decoded_bytes(&entry, FactsNeed::Index), None);
        assert_eq!(
            budget(&successor),
            (0, 0, 0, unbuilt_bytes.len() + full_bytes.len())
        );
        assert!(dead.exists());
        drop(successor);
        fs::remove_dir_all(namespace).unwrap();
    }

    #[test]
    fn an_inherited_unbuilt_entry_upgrades_into_the_current_directory() {
        for unlink_fails in [false, true] {
            let namespace = isolated_namespace();
            let compact = index_facts(3, 2);
            let full = pushed_facts(3, 2);
            let entry = revision(1);
            let other = revision(9);
            let live = reuse_cache(&namespace, 64 * 1024);
            assert!(live
                .insert(&other, &full, |_| true, |_| true)
                .unwrap()
                .serves());
            let live_file = live.entry_file(&other);
            let first = reuse_cache(&namespace, 64 * 1024);
            assert!(first
                .insert(&entry, &compact, |_| true, |_| true)
                .unwrap()
                .serves());
            let old_path = first.entry_file(&entry);
            let old_size = fs::metadata(&old_path).unwrap().len() as usize;
            let dead = first.dir.clone();
            drop(first);
            let second = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(budget(&second), (1, 0, old_size, 0));
            assert!(!second.lists(&entry));
            if unlink_fails {
                strand_old_name(&second, dead.clone());
            }
            assert!(second
                .insert(&entry, &full, |_| true, |_| true)
                .unwrap()
                .serves());
            second.set_upgrade_hook(None);
            let new_size = second.entry_file_len(&entry) as usize;
            assert!(second.lists(&entry), "the upgraded entry is not owned");
            assert_eq!(text_state_of(&second, &entry), Some(TextState::Present));
            assert_eq!(second.stats().writes, 1);
            assert_eq!(entry_files(&second), [second.entry_file(&entry)]);
            if unlink_fails {
                assert_eq!(
                    budget(&second),
                    (1, new_size, 0, old_size),
                    "the stuck inherited name is not claimed garbage"
                );
                assert!(old_path.is_dir());
                let garbage: Vec<(CString, usize)> = second
                    .state
                    .lock()
                    .unwrap()
                    .claimed
                    .iter()
                    .map(|owner| (owner.name.clone(), owner.garbage))
                    .collect();
                assert_eq!(garbage, [(file_name(&dead), old_size)]);
            } else {
                assert_eq!(budget(&second), (1, new_size, 0, 0));
                assert!(!old_path.exists());
                assert!(!dead.exists(), "the emptied dead owner survived");
            }
            assert!(stranded(&second).is_empty());
            assert!(live_file.is_file());
            assert!(live.has_entry(&other).unwrap());
            let DiskLookup::Hit(loaded) =
                lookup(&second, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
            else {
                panic!("expected full facts");
            };
            assert!(loaded.is_full());
            drop(second);
            drop(live);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn restart_keeps_the_full_name_over_a_crashed_upgrade_leftover() {
        let namespace = isolated_namespace();
        let compact = index_facts(3, 2);
        let full = pushed_facts(3, 2);
        let entry = revision(1);
        let first = reuse_cache(&namespace, 64 * 1024);
        assert!(first
            .insert(&entry, &compact, |_| true, |_| true)
            .unwrap()
            .serves());
        let unbuilt_path = first.entry_file(&entry);
        let unbuilt_bytes = fs::read(&unbuilt_path).unwrap();
        assert!(first
            .insert(&entry, &full, |_| true, |_| true)
            .unwrap()
            .serves());
        let full_path = first.entry_file(&entry);
        let full_bytes = fs::read(&full_path).unwrap();
        assert!(!unbuilt_path.exists());
        let (unbuilt_name, full_name) = (file_name(&unbuilt_path), file_name(&full_path));
        planted_bytes(&first.dir, &unbuilt_name, &unbuilt_bytes);
        planted(&first.dir, c"abc.tmp", 10);
        let dead = first.dir.clone();
        drop(first);
        let second = reuse_cache(&namespace, 64 * 1024);
        assert_eq!(
            budget(&second),
            (1, 0, full_bytes.len(), unbuilt_bytes.len())
        );
        assert_eq!(text_state_of(&second, &entry), Some(TextState::Present));
        let DiskLookup::Hit(loaded) =
            lookup(&second, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("expected full facts");
        };
        assert!(loaded.is_full());
        assert_eq!(budget(&second), (1, full_bytes.len(), 0, 0));
        assert!(!unbuilt_path.exists());
        assert!(!dead.exists());
        drop(second);
        fs::remove_dir_all(&namespace).unwrap();
        for (older, newer) in [
            ((&unbuilt_name, &unbuilt_bytes), (&full_name, &full_bytes)),
            ((&full_name, &full_bytes), (&unbuilt_name, &unbuilt_bytes)),
        ] {
            let namespace = isolated_namespace();
            let owner = reuse_cache(&namespace, 64 * 1024);
            let newer_path = planted_bytes(&owner.dir, newer.0, newer.1);
            let owner_dir = owner.dir.clone();
            drop(owner);
            let dead = dead_owner(&namespace);
            let older_path = planted_bytes(&dead, older.0, older.1);
            let successor = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(
                budget(&successor),
                (1, 0, full_bytes.len(), unbuilt_bytes.len())
            );
            assert_eq!(text_state_of(&successor, &entry), Some(TextState::Present));
            let (loser_dir, winner_path) = if older.0 == &full_name {
                (file_name(&owner_dir), older_path)
            } else {
                (file_name(&dead), newer_path)
            };
            let garbage: Vec<(CString, usize)> = successor
                .state
                .lock()
                .unwrap()
                .claimed
                .iter()
                .filter(|owner| owner.garbage > 0)
                .map(|owner| (owner.name.clone(), owner.garbage))
                .collect();
            assert_eq!(garbage, [(loser_dir, unbuilt_bytes.len())]);
            let DiskLookup::Hit(loaded) = lookup(
                &successor,
                &entry,
                &mut DiskRead::default(),
                FactsNeed::Full,
            )
            .unwrap() else {
                panic!("expected full facts");
            };
            assert!(loaded.is_full());
            assert!(!winner_path.exists(), "the winner was not moved");
            assert!(successor.lists(&entry));
            assert_eq!(budget(&successor).1, full_bytes.len());
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn every_text_state_restarts_with_its_own_bounds() {
        let mut exceeded = pushed_facts(2, 2);
        exceeded.overrides = OverrideFacts::Exceeded;
        exceeded.refresh_accounted();
        for (facts, state) in [
            (index_facts(2, 2), TextState::Unbuilt),
            (pushed_facts(2, 2), TextState::Present),
            (exceeded, TextState::Exceeded),
        ] {
            let namespace = isolated_namespace();
            let entry = revision(1);
            let bounds = |cache: &PreparedDiskCache| {
                (
                    cache.read_bound(&entry, FactsNeed::Index),
                    cache.read_bound(&entry, FactsNeed::Full),
                    cache.decoded_bytes(&entry, FactsNeed::Index),
                    cache.decoded_bytes(&entry, FactsNeed::Full),
                    text_state_of(cache, &entry),
                )
            };
            let first = reuse_cache(&namespace, 64 * 1024);
            assert!(first
                .insert(&entry, &facts, |_| true, |_| true)
                .unwrap()
                .serves());
            let expected = bounds(&first);
            assert_eq!(expected.4, Some(state));
            drop(first);
            let second = reuse_cache(&namespace, 64 * 1024);
            assert_eq!(bounds(&second), expected, "{state:?}");
            let lookup =
                lookup(&second, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap();
            match (state, lookup) {
                (TextState::Unbuilt, DiskLookup::IndexOnly(loaded)) => assert!(index_only(&loaded)),
                (TextState::Present, DiskLookup::Hit(loaded)) => {
                    assert!(matches!(loaded.overrides, OverrideFacts::Loaded(_)))
                }
                (TextState::Exceeded, DiskLookup::Hit(loaded)) => {
                    assert!(matches!(loaded.overrides, OverrideFacts::Exceeded))
                }
                _ => panic!("{state:?} restarted into the wrong lookup"),
            }
            assert!(second.lists(&entry));
            drop(second);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn core_carries_a_build_identity() {
        assert_eq!(crate::BUILD_IDENTITY.len(), 64);
        assert!(hex_bytes(crate::BUILD_IDENTITY.as_bytes()));
    }
    #[test]
    fn typed_skills_disk_capability_is_never_reported_as_index_or_full() {
        let cache = cache(2 * 1024 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        let weak = PreparedFacts::skills(crate::snapshot_prepared::SkillEvidence::Parsed {
            skills: Vec::new(),
        });
        assert!(cache
            .insert(&entry, &weak, |_| true, |_| true)
            .unwrap()
            .serves());
        let DiskLookup::Hit(loaded) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Skills).unwrap()
        else {
            panic!("Skills must satisfy its exact demand")
        };
        assert!(loaded.serves(FactsNeed::Skills));
        assert!(!loaded.serves(FactsNeed::Index));
        assert!(!loaded.serves(FactsNeed::Full));
        assert!(!index_only(&loaded));
        assert!(cache.lists_for(&entry, FactsNeed::Skills));
        for need in [FactsNeed::Index, FactsNeed::Full] {
            assert!(!cache.lists_for(&entry, need));
            assert!(!cache.touch(&entry, need, |_| true).unwrap());
            assert!(matches!(
                lookup(&cache, &entry, &mut DiskRead::default(), need).unwrap(),
                DiskLookup::SkillsOnly(_)
            ));
        }
        assert!(cache
            .insert(&entry, &facts(), |_| true, |_| true)
            .unwrap()
            .serves());
        assert!(cache
            .insert(&entry, &weak, |_| true, |_| true)
            .unwrap()
            .serves());
        let DiskLookup::Hit(complete) =
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap()
        else {
            panic!("weaker publication must retain Full")
        };
        assert!(complete.serves(FactsNeed::Full));
        assert_eq!(
            complete
                .query(&json!({"kind":"has_tool","pattern":"Read"}))
                .unwrap()["value"]
                .as_bool(),
            Some(true)
        );
    }

    #[test]
    fn typed_skills_inherited_stronger_evidence_is_not_downgraded() {
        for need in [FactsNeed::Index, FactsNeed::Full] {
            let namespace = isolated_namespace();
            let entry = revision(1);
            let mut strong = facts();
            if need == FactsNeed::Index {
                strong.overrides = OverrideFacts::Unloaded;
                strong.refresh_accounted();
            }
            let old = left_behind(&namespace, &entry, &strong);
            let bytes = fs::read(&old).unwrap();
            let successor = reuse_cache(&namespace, 64 * 1024);
            let inherited = budget(&successor);
            assert_eq!(inherited, (1, 0, bytes.len(), 0));
            let weak = PreparedFacts::skills(crate::snapshot_prepared::SkillEvidence::Parsed {
                skills: Vec::new(),
            });
            assert!(successor
                .insert(&entry, &weak, |_| true, |_| true)
                .unwrap()
                .serves());
            assert_eq!(budget(&successor), inherited);
            assert_eq!(fs::read(&old).unwrap(), bytes);
            let DiskLookup::Hit(loaded) =
                lookup(&successor, &entry, &mut DiskRead::default(), need).unwrap()
            else {
                panic!("inherited stronger evidence must retain its capability")
            };
            assert_eq!(loaded.capability(), need);
            assert_eq!(loaded.index_inputs(), strong.index_inputs());
            drop(successor);
            fs::remove_dir_all(namespace).unwrap();
        }
    }

    #[test]
    fn typed_skills_unknown_disk_capability_is_rejected_before_payload_acceptance() {
        let cache = cache(2 * 1024 * 1024);
        let entry = key(stamp(1), &json!({"root":"/repo"}));
        cache.insert(&entry, &facts(), |_| true, |_| true).unwrap();
        let path = cache.entry_path(entry.digest);
        let mut bytes = fs::read(&path).unwrap();
        bytes[137] = 3;
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            lookup(&cache, &entry, &mut DiskRead::default(), FactsNeed::Full).unwrap(),
            DiskLookup::Retired
        ));
    }
}

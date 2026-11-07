#[cfg(test)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SidechainCalls {
    listings: usize,
    resolutions: usize,
    opens: usize,
    stats: usize,
}

#[cfg(test)]
thread_local! {
    static SIDECHAIN_CALLS: Cell<SidechainCalls> = const {
        Cell::new(SidechainCalls { listings: 0, resolutions: 0, opens: 0, stats: 0 })
    };
}

#[cfg(test)]
fn count_sidechain_call(call: fn(&mut SidechainCalls) -> &mut usize) {
    let mut calls = SIDECHAIN_CALLS.get();
    *call(&mut calls) += 1;
    SIDECHAIN_CALLS.set(calls);
}

pub fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0,
        "descriptor limit"
    );
    limit.rlim_cur = descriptor_ceiling(limit.rlim_max);
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) },
        0,
        "raise descriptor limit"
    );
}

#[cfg(target_vendor = "apple")]
fn descriptor_ceiling(hard: libc::rlim_t) -> libc::rlim_t {
    let mut per_process: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    assert_eq!(
        unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&mut per_process as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        },
        0,
        "kern.maxfilesperproc"
    );
    hard.min(per_process as libc::rlim_t)
}

#[cfg(not(target_vendor = "apple"))]
fn descriptor_ceiling(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

fn sidechain_stat(path: &Path) -> std::io::Result<SourceStamp> {
    #[cfg(test)]
    count_sidechain_call(|calls| &mut calls.stats);
    std::fs::metadata(path).map(|metadata| SourceStamp::of(&metadata))
}

fn sidechain_realpath(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    count_sidechain_call(|calls| &mut calls.resolutions);
    realpath(path)
}

fn sidechain_listing(path: &Path) -> Result<OpenDirectory, SnapshotError> {
    #[cfg(test)]
    count_sidechain_call(|calls| &mut calls.listings);
    OpenDirectory::open(path)
}

fn sidechain_name(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.ends_with(b".jsonl") && !name.starts_with(b"._")
}

#[cfg_attr(target_vendor = "apple", allow(dead_code))]
enum FeedChange {
    Overflow,
    Target { key: u64, gone: bool },
    Entry { key: u64, name: OsString },
}

enum FeedTarget {
    Directory(PathBuf),
    Member(PathBuf, OsString),
}

struct FeedMember {
    inode: u64,
    stamp: Option<SourceStamp>,
    watch: Option<(sidechain_queue::WatchHandle, u64)>,
}

struct FeedDirectory {
    epoch: u64,
    key: u64,
    watch: sidechain_queue::WatchHandle,
    stamp: SourceStamp,
    members: std::collections::BTreeMap<OsString, FeedMember>,
    holders: HashSet<SourceIdentity>,
    complete: bool,
    stale: bool,
    rescan: bool,
}

#[derive(Default)]
struct FeedState {
    queue: Option<sidechain_queue::WatchQueue>,
    directories: HashMap<PathBuf, FeedDirectory>,
    targets: HashMap<u64, FeedTarget>,
    roots: HashMap<SourceIdentity, PathBuf>,
    next_key: u64,
    warned: HashSet<PathBuf>,
    failures: Vec<(PathBuf, String)>,
}

#[derive(Default)]
struct FeedShared {
    state: Mutex<FeedState>,
    releases: Mutex<Vec<SourceIdentity>>,
    stopping: AtomicBool,
}

struct FeedWatcher {
    wake: std::os::fd::OwnedFd,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct SidechainFeed {
    shared: Arc<FeedShared>,
    watcher: std::sync::OnceLock<FeedWatcher>,
}

impl Drop for SidechainFeed {
    fn drop(&mut self) {
        let Some(mut watcher) = self.watcher.take() else {
            return;
        };
        self.shared.stopping.store(true, Ordering::Release);
        wake(&watcher.wake);
        watcher
            .thread
            .take()
            .expect("sidechain feed thread")
            .join()
            .expect("sidechain feed thread");
    }
}

fn wake(descriptor: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    unsafe { libc::write(descriptor.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
}

fn wake_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut ends = [0; 2];
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for end in ends {
        unsafe {
            libc::fcntl(end, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(end, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    Ok(unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(ends[0]),
            std::os::fd::OwnedFd::from_raw_fd(ends[1]),
        )
    })
}

impl SidechainFeed {
    pub(crate) fn drain(&self) {
        self.shared.drain();
    }

    pub(crate) fn release(&self, root: SourceIdentity) {
        let Some(watcher) = self.watcher.get() else {
            return;
        };
        self.shared.releases.lock().expect("sidechain releases").push(root);
        wake(&watcher.wake);
    }

    pub(crate) fn listing(
        &self,
        root: SourceIdentity,
        directory: &Path,
        live: bool,
    ) -> Option<(SourceStamp, Vec<PathBuf>)> {
        let mut state = self.shared.state.lock().expect("sidechain feed");
        let watched = state
            .directories
            .get_mut(directory)
            .filter(|watched| watched.complete)?;
        if live {
            watched.holders.insert(root);
        }
        let listed = (
            watched.stamp,
            watched
                .members
                .keys()
                .map(|name| directory.join(name))
                .collect(),
        );
        if live {
            state.roots.insert(root, directory.to_owned());
        }
        Some(listed)
    }

    pub(crate) fn stamp(&self, path: &Path) -> Option<SourceStamp> {
        let state = self.shared.state.lock().expect("sidechain feed");
        let member = state
            .directories
            .get(path.parent()?)
            .filter(|watched| watched.complete)?
            .members
            .get(path.file_name()?)?;
        member.watch.as_ref()?;
        member.stamp
    }

    pub(crate) fn begin(&self, root: SourceIdentity, directory: &Path) -> Option<u64> {
        let mut state = self.shared.state.lock().expect("sidechain feed");
        if let Some(watched) = state.directories.get_mut(directory) {
            if watched.complete {
                return None;
            }
            watched.holders.insert(root);
            let epoch = watched.epoch;
            state.roots.insert(root, directory.to_owned());
            return Some(epoch);
        }
        if state.queue.is_none() {
            match sidechain_queue::WatchQueue::new() {
                Ok(queue) => state.queue = Some(queue),
                Err(error) => {
                    state.warn(directory, &error.to_string());
                    return None;
                }
            }
        }
        let descriptor = state.queue.as_ref().expect("sidechain queue").descriptor();
        if self.watcher.get().is_none() {
            let shared = Arc::clone(&self.shared);
            let started = wake_pipe().and_then(|(read, write)| {
                std::thread::Builder::new()
                    .name("cc-transcript-sidechain-feed".to_owned())
                    .spawn(move || shared.follow(descriptor, read))
                    .map(|thread| FeedWatcher {
                        wake: write,
                        thread: Some(thread),
                    })
            });
            match started {
                Ok(watcher) => {
                    assert!(self.watcher.set(watcher).is_ok(), "one sidechain feed thread");
                }
                Err(error) => {
                    state.warn(directory, &error.to_string());
                    return None;
                }
            }
        }
        let epoch = state.next_key();
        let watch = state
            .queue
            .as_ref()
            .expect("sidechain queue")
            .watch_directory(directory, epoch);
        let (watch, key) = match watch {
            Ok(watch) => watch,
            Err(error) => {
                state.warn(directory, &error.to_string());
                return None;
            }
        };
        let stamp = match sidechain_stat(directory) {
            Ok(stamp) => stamp,
            Err(error) => {
                state.queue.as_ref().expect("sidechain queue").unwatch(watch);
                state.warn(directory, &error.to_string());
                return None;
            }
        };
        state
            .targets
            .insert(key, FeedTarget::Directory(directory.to_owned()));
        state.roots.insert(root, directory.to_owned());
        state.directories.insert(
            directory.to_owned(),
            FeedDirectory {
                epoch,
                key,
                watch,
                stamp,
                members: std::collections::BTreeMap::new(),
                holders: HashSet::from([root]),
                complete: false,
                stale: false,
                rescan: false,
            },
        );
        Some(epoch)
    }

    pub(crate) fn watch(&self, epoch: u64, path: &Path, canonical: &Path) {
        let mut state = self.shared.state.lock().expect("sidechain feed");
        let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        if !state.directories.get(directory).is_some_and(|watched| {
            watched.epoch == epoch && !watched.complete && !watched.members.contains_key(name)
        }) {
            return;
        }
        let watch = if canonical == path {
            state.watch_member(directory, name)
        } else {
            state.warn(path, "sidechain resolves outside its directory");
            None
        };
        state
            .directories
            .get_mut(directory)
            .expect("watched sidechain directory")
            .members
            .insert(
                name.to_owned(),
                FeedMember {
                    inode: 0,
                    stamp: None,
                    watch,
                },
            );
    }

    pub(crate) fn record(&self, epoch: u64, path: &Path, stamp: SourceStamp) {
        let mut state = self.shared.state.lock().expect("sidechain feed");
        let Some(member) = path
            .parent()
            .and_then(|directory| state.directories.get_mut(directory))
            .filter(|watched| watched.epoch == epoch && !watched.complete)
            .and_then(|watched| watched.members.get_mut(path.file_name()?))
            .filter(|member| member.watch.is_some())
        else {
            return;
        };
        member.inode = stamp.identity.inode;
        member.stamp.get_or_insert(stamp);
    }

    pub(crate) fn complete(&self, epoch: u64) {
        let mut state = self.shared.state.lock().expect("sidechain feed");
        if let Some(watched) = state
            .directories
            .values_mut()
            .find(|watched| watched.epoch == epoch)
        {
            watched.complete = true;
        }
    }

    pub(crate) fn take_failures(&self) -> Vec<(PathBuf, String)> {
        std::mem::take(&mut self.shared.state.lock().expect("sidechain feed").failures)
    }

    #[cfg(test)]
    pub(crate) fn descriptors(&self) -> usize {
        let state = self.shared.state.lock().expect("sidechain feed");
        state
            .directories
            .values()
            .map(|watched| {
                1 + watched
                    .members
                    .values()
                    .filter(|member| member.watch.is_some())
                    .count()
            })
            .sum()
    }
}

impl FeedShared {
    fn follow(&self, queue: std::os::fd::RawFd, wake: std::os::fd::OwnedFd) {
        use std::os::fd::AsRawFd;
        let mut ready = [
            libc::pollfd {
                fd: queue,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            if unsafe { libc::poll(ready.as_mut_ptr(), 2, -1) } < 0 {
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted,
                    "sidechain feed poll: {error}"
                );
                continue;
            }
            if ready[1].revents != 0 {
                let mut buffer = [0u8; 64];
                while unsafe { libc::read(wake.as_raw_fd(), buffer.as_mut_ptr().cast(), 64) } > 0 {}
            }
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            self.drain();
        }
    }

    fn drain(&self) {
        let releases = std::mem::take(&mut *self.releases.lock().expect("sidechain releases"));
        let mut state = self.state.lock().expect("sidechain feed");
        for root in releases {
            state.release(root);
        }
        let Some(changes) = state.queue.as_ref().map(sidechain_queue::WatchQueue::read) else {
            return;
        };
        for change in changes {
            state.apply(change);
        }
        let stale: Vec<PathBuf> = state
            .directories
            .iter()
            .filter(|(_, watched)| watched.complete && watched.stale)
            .map(|(path, _)| path.clone())
            .collect();
        for path in stale {
            state.relist(&path);
        }
    }
}

impl FeedState {
    fn next_key(&mut self) -> u64 {
        self.next_key += 1;
        self.next_key
    }

    fn warn(&mut self, path: &Path, reason: &str) {
        if self.warned.insert(path.to_owned()) {
            self.failures.push((path.to_owned(), reason.to_owned()));
        }
    }

    fn watch_member(
        &mut self,
        directory: &Path,
        name: &OsStr,
    ) -> Option<(sidechain_queue::WatchHandle, u64)> {
        let path = directory.join(name);
        let proposed = self.next_key();
        match self
            .queue
            .as_ref()
            .expect("sidechain queue")
            .watch_member(&path, proposed)
        {
            Ok((_, key)) if self.targets.contains_key(&key) => {
                self.warn(&path, "sidechain shares its inode with another watched file");
                None
            }
            Ok((handle, key)) => {
                self.targets.insert(
                    key,
                    FeedTarget::Member(directory.to_owned(), name.to_owned()),
                );
                Some((handle, key))
            }
            Err(error) => {
                self.warn(&path, &error.to_string());
                None
            }
        }
    }

    fn release(&mut self, root: SourceIdentity) {
        let Some(directory) = self.roots.remove(&root) else {
            return;
        };
        let Some(watched) = self.directories.get_mut(&directory) else {
            return;
        };
        watched.holders.remove(&root);
        if watched.holders.is_empty() {
            self.drop_directory(&directory);
        }
    }

    fn apply(&mut self, change: FeedChange) {
        match change {
            FeedChange::Overflow => {
                for watched in self.directories.values_mut() {
                    watched.stale = true;
                    watched.rescan = true;
                }
            }
            FeedChange::Target { key, gone } => match self.targets.get(&key) {
                Some(FeedTarget::Directory(directory)) => {
                    let directory = directory.clone();
                    if gone {
                        self.drop_directory(&directory);
                    } else if let Some(watched) = self.directories.get_mut(&directory) {
                        watched.stale = true;
                    }
                }
                Some(FeedTarget::Member(directory, name)) => {
                    let (directory, name) = (directory.clone(), name.clone());
                    if gone {
                        self.remove_member(&directory, &name);
                        if let Some(watched) = self.directories.get_mut(&directory) {
                            watched.stale = true;
                        }
                    } else {
                        self.restat(&directory, &name);
                    }
                }
                None => {}
            },
            FeedChange::Entry { key, name } => {
                let Some(FeedTarget::Directory(directory)) = self.targets.get(&key) else {
                    return;
                };
                if !sidechain_name(&name) {
                    return;
                }
                let directory = directory.clone();
                if let Some(watched) = self.directories.get_mut(&directory) {
                    watched.stale = true;
                }
            }
        }
    }

    fn restat(&mut self, directory: &Path, name: &OsStr) {
        let stamp = sidechain_stat(&directory.join(name));
        let Some(watched) = self.directories.get_mut(directory) else {
            return;
        };
        match stamp {
            Ok(stamp) => {
                if let Some(member) = watched.members.get_mut(name) {
                    member.inode = stamp.identity.inode;
                    member.stamp = Some(stamp);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                watched.stale = true;
                self.remove_member(directory, name);
            }
            Err(_) => {
                if let Some(member) = watched.members.get_mut(name) {
                    member.stamp = None;
                }
            }
        }
    }

    fn relist(&mut self, directory: &Path) {
        let entries = sidechain_listing(directory).and_then(|listing| {
            let stamp = listing.stamp();
            listing
                .filter(|entry| entry.as_ref().map_or(true, |entry| sidechain_name(&entry.name)))
                .map(|entry| {
                    entry
                        .map(|entry| (entry.name, entry.inode, entry.kind))
                        .map_err(io_error)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|entries| (stamp, entries))
        });
        let Ok((stamp, entries)) = entries else {
            self.drop_directory(directory);
            return;
        };
        let watched = self
            .directories
            .get_mut(directory)
            .expect("stale sidechain directory");
        watched.stamp = stamp;
        watched.stale = false;
        let rescan = std::mem::take(&mut watched.rescan);
        let present: HashSet<&OsString> = entries.iter().map(|(name, _, _)| name).collect();
        let vanished: Vec<OsString> = watched
            .members
            .keys()
            .filter(|name| !present.contains(name))
            .cloned()
            .collect();
        for name in vanished {
            self.remove_member(directory, &name);
        }
        for (name, inode, kind) in entries {
            let known = self.directories[directory]
                .members
                .get(&name)
                .is_some_and(|member| member.inode == inode);
            if known {
                if rescan {
                    self.restat(directory, &name);
                }
                continue;
            }
            self.remove_member(directory, &name);
            self.add_member(directory, name, inode, kind);
        }
    }

    fn add_member(&mut self, directory: &Path, name: OsString, inode: u64, kind: u8) {
        let path = directory.join(&name);
        let link = kind == libc::DT_LNK
            || (kind == libc::DT_UNKNOWN
                && std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink()));
        let watch = if link {
            self.warn(&path, "sidechain resolves outside its directory");
            None
        } else {
            self.watch_member(directory, &name)
        };
        let stamp = match watch.as_ref().map(|_| sidechain_stat(&path)) {
            Some(Ok(stamp)) => Some(stamp),
            Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let (handle, key) = watch.expect("sidechain member watch");
                self.targets.remove(&key);
                self.queue.as_ref().expect("sidechain queue").unwatch(handle);
                return;
            }
            _ => None,
        };
        self.directories
            .get_mut(directory)
            .expect("watched sidechain directory")
            .members
            .insert(
                name,
                FeedMember {
                    inode,
                    stamp,
                    watch,
                },
            );
    }

    fn remove_member(&mut self, directory: &Path, name: &OsStr) {
        let Some(member) = self
            .directories
            .get_mut(directory)
            .and_then(|watched| watched.members.remove(name))
        else {
            return;
        };
        if let Some((handle, key)) = member.watch {
            self.targets.remove(&key);
            self.queue.as_ref().expect("sidechain queue").unwatch(handle);
        }
    }

    fn drop_directory(&mut self, directory: &Path) {
        let Some(watched) = self.directories.remove(directory) else {
            return;
        };
        let queue = self.queue.as_ref().expect("sidechain queue");
        for (handle, key) in watched.members.into_values().filter_map(|member| member.watch) {
            self.targets.remove(&key);
            queue.unwatch(handle);
        }
        self.targets.remove(&watched.key);
        queue.unwatch(watched.watch);
        self.roots.retain(|_, watching| watching != directory);
        self.warned.retain(|path| !path.starts_with(directory));
    }
}

#[cfg(target_vendor = "apple")]
mod sidechain_queue {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::FeedChange;

    const EVENTS: u32 = libc::NOTE_DELETE
        | libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_ATTRIB
        | libc::NOTE_LINK
        | libc::NOTE_RENAME
        | libc::NOTE_REVOKE;
    const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;

    pub(super) struct WatchQueue {
        queue: OwnedFd,
    }

    pub(super) struct WatchHandle {
        _descriptor: OwnedFd,
    }

    impl WatchQueue {
        pub(super) fn new() -> std::io::Result<Self> {
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Err(std::io::Error::last_os_error());
            }
            unsafe { libc::fcntl(queue, libc::F_SETFD, libc::FD_CLOEXEC) };
            Ok(Self {
                queue: unsafe { OwnedFd::from_raw_fd(queue) },
            })
        }

        pub(super) fn descriptor(&self) -> RawFd {
            self.queue.as_raw_fd()
        }

        fn watch(&self, path: &Path, key: u64) -> std::io::Result<WatchHandle> {
            let encoded = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
            #[cfg(test)]
            super::count_sidechain_call(|calls| &mut calls.opens);
            let descriptor = unsafe { libc::open(encoded.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let handle = WatchHandle {
                _descriptor: unsafe { OwnedFd::from_raw_fd(descriptor) },
            };
            let change = libc::kevent {
                ident: descriptor as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: EVENTS,
                data: 0,
                udata: key as usize as *mut libc::c_void,
            };
            let added = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if added != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(handle)
        }

        pub(super) fn watch_directory(
            &self,
            path: &Path,
            key: u64,
        ) -> std::io::Result<(WatchHandle, u64)> {
            Ok((self.watch(path, key)?, key))
        }

        pub(super) fn watch_member(
            &self,
            path: &Path,
            key: u64,
        ) -> std::io::Result<(WatchHandle, u64)> {
            Ok((self.watch(path, key)?, key))
        }

        pub(super) fn unwatch(&self, _handle: WatchHandle) {}

        pub(super) fn read(&self) -> Vec<FeedChange> {
            const BATCH: usize = 64;
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let mut events: [libc::kevent; BATCH] = unsafe { std::mem::zeroed() };
            let mut changes = Vec::new();
            loop {
                let count = unsafe {
                    libc::kevent(
                        self.queue.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        BATCH as libc::c_int,
                        &zero,
                    )
                };
                if count < 0 {
                    let error = std::io::Error::last_os_error();
                    assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted,
                        "sidechain kqueue: {error}"
                    );
                    continue;
                }
                let count = count as usize;
                changes.extend(events[..count].iter().map(|event| FeedChange::Target {
                    key: event.udata as usize as u64,
                    gone: event.fflags & GONE != 0,
                }));
                if count < BATCH {
                    return changes;
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sidechain_queue {
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use super::FeedChange;

    const DIRECTORY_EVENTS: u32 = libc::IN_ATTRIB
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR
        | libc::IN_DONT_FOLLOW
        | libc::IN_EXCL_UNLINK;
    const MEMBER_EVENTS: u32 = libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_DONT_FOLLOW;
    const GONE: u32 = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED | libc::IN_UNMOUNT;

    pub(super) struct WatchQueue {
        queue: OwnedFd,
    }

    pub(super) struct WatchHandle {
        descriptor: i32,
    }

    impl WatchQueue {
        pub(super) fn new() -> std::io::Result<Self> {
            let queue = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if queue < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self {
                queue: unsafe { OwnedFd::from_raw_fd(queue) },
            })
        }

        pub(super) fn descriptor(&self) -> RawFd {
            self.queue.as_raw_fd()
        }

        fn watch(&self, path: &Path, events: u32) -> std::io::Result<(WatchHandle, u64)> {
            let encoded = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
            #[cfg(test)]
            super::count_sidechain_call(|calls| &mut calls.opens);
            let descriptor =
                unsafe { libc::inotify_add_watch(self.queue.as_raw_fd(), encoded.as_ptr(), events) };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((WatchHandle { descriptor }, descriptor as u64))
        }

        pub(super) fn watch_directory(
            &self,
            path: &Path,
            _key: u64,
        ) -> std::io::Result<(WatchHandle, u64)> {
            self.watch(path, DIRECTORY_EVENTS)
        }

        pub(super) fn watch_member(
            &self,
            path: &Path,
            _key: u64,
        ) -> std::io::Result<(WatchHandle, u64)> {
            self.watch(path, MEMBER_EVENTS)
        }

        pub(super) fn unwatch(&self, handle: WatchHandle) {
            unsafe { libc::inotify_rm_watch(self.queue.as_raw_fd(), handle.descriptor) };
        }

        pub(super) fn read(&self) -> Vec<FeedChange> {
            const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
            let mut buffer = vec![0u8; 64 * 1024];
            let mut changes = Vec::new();
            loop {
                let read = unsafe {
                    libc::read(
                        self.queue.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if read < 0 {
                    let error = std::io::Error::last_os_error();
                    match error.kind() {
                        std::io::ErrorKind::WouldBlock => return changes,
                        std::io::ErrorKind::Interrupted => continue,
                        _ => panic!("sidechain inotify: {error}"),
                    }
                }
                let mut offset = 0;
                while offset < read as usize {
                    let event: libc::inotify_event =
                        unsafe { std::ptr::read_unaligned(buffer[offset..].as_ptr().cast()) };
                    let name = &buffer[offset + HEADER..offset + HEADER + event.len as usize];
                    offset += HEADER + event.len as usize;
                    let key = event.wd as u64;
                    if event.mask & libc::IN_Q_OVERFLOW != 0 {
                        changes.push(FeedChange::Overflow);
                    } else if event.len == 0 {
                        changes.push(FeedChange::Target {
                            key,
                            gone: event.mask & GONE != 0,
                        });
                    } else {
                        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
                        changes.push(FeedChange::Entry {
                            key,
                            name: OsString::from_vec(name.to_vec()),
                        });
                    }
                }
            }
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
mod sidechain_queue {
    use std::os::fd::RawFd;
    use std::path::Path;

    use super::FeedChange;

    pub(super) struct WatchQueue;

    pub(super) struct WatchHandle;

    impl WatchQueue {
        pub(super) fn new() -> std::io::Result<Self> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }

        pub(super) fn descriptor(&self) -> RawFd {
            unreachable!("no sidechain queue on this platform")
        }

        pub(super) fn watch_directory(&self, _: &Path, _: u64) -> std::io::Result<(WatchHandle, u64)> {
            unreachable!("no sidechain queue on this platform")
        }

        pub(super) fn watch_member(&self, _: &Path, _: u64) -> std::io::Result<(WatchHandle, u64)> {
            unreachable!("no sidechain queue on this platform")
        }

        pub(super) fn unwatch(&self, _: WatchHandle) {}

        pub(super) fn read(&self) -> Vec<FeedChange> {
            unreachable!("no sidechain queue on this platform")
        }
    }
}

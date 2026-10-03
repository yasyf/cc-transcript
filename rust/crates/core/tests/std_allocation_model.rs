use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const TRACE_SLOTS: usize = 32;

struct Recording;

thread_local! {
    static RECORD: RefCell<(bool, usize, [usize; TRACE_SLOTS])> =
        const { RefCell::new((false, 0, [0; TRACE_SLOTS])) };
}

fn record(size: usize) {
    let _ = RECORD.try_with(|record| {
        if let Ok(mut record) = record.try_borrow_mut() {
            let (armed, count, sizes) = &mut *record;
            if *armed {
                if *count < TRACE_SLOTS {
                    sizes[*count] = size;
                }
                *count += 1;
            }
        }
    });
}

unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

fn allocations<R>(op: impl FnOnce() -> R) -> (R, Vec<usize>) {
    RECORD.with(|record| *record.borrow_mut() = (true, 0, [0; TRACE_SLOTS]));
    let result = op();
    let (_, count, sizes) = RECORD
        .with(|record| std::mem::replace(&mut *record.borrow_mut(), (false, 0, [0; TRACE_SLOTS])));
    assert!(
        count <= TRACE_SLOTS,
        "{count} allocations overflowed the trace"
    );
    (result, sizes[..count].to_vec())
}

fn sizes<R>(op: impl FnOnce() -> R) -> Vec<usize> {
    allocations(op).1
}

fn temp_directory(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("cc-std-model-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir(&directory).unwrap();
    directory
}

fn joined_in_place(base: &Path, stem: &OsStr) -> PathBuf {
    let mut directory =
        PathBuf::with_capacity(base.as_os_str().len() + 1 + stem.len() + 1 + "subagents".len());
    directory.push(base);
    directory.push(stem);
    directory.push("subagents");
    directory
}

#[test]
fn arc_new_allocates_the_padded_inner_layout() {
    assert_eq!(sizes(|| Arc::new(7u8)), [24]);
    assert_eq!(sizes(|| Arc::new([0u8; 3])), [24]);
    assert_eq!(sizes(|| Arc::new(0u128)), [32]);
    assert_eq!(sizes(|| Arc::new([0u64; 7])), [72]);
}

#[test]
fn arc_slices_allocate_their_header_even_when_empty() {
    let empty = Vec::<u64>::new();
    assert_eq!(sizes(move || Arc::<[u64]>::from(empty)), [16]);
    let bytes = vec![1u8, 2, 3];
    assert_eq!(sizes(move || Arc::<[u8]>::from(bytes)), [24]);
    let key = "k".repeat(64);
    assert_eq!(sizes(move || Arc::<str>::from(key)), [80]);
    let reserved = Vec::<(PathBuf, u64)>::with_capacity(9);
    assert_eq!(sizes(move || Arc::<[(PathBuf, u64)]>::from(reserved)), [16]);
}

#[test]
fn mutex_storage_is_boxed_once_on_first_lock() {
    let (mutex, construction) = allocations(|| Mutex::new(0u8));
    assert!(construction.is_empty(), "{construction:?}");
    let first = sizes(|| drop(mutex.lock().unwrap()));
    if cfg!(target_vendor = "apple") {
        assert_eq!(first, [64]);
    } else if cfg!(target_os = "linux") {
        assert!(first.is_empty(), "{first:?}");
    }
    let second = sizes(|| drop(mutex.lock().unwrap()));
    assert!(second.is_empty(), "{second:?}");
}

#[test]
fn read_dir_allocates_its_root_copy_and_inner_arc() {
    let root = temp_directory("read-dir");
    assert!(root.as_os_str().len() < 384);
    let (entries, allocated) = allocations(|| std::fs::read_dir(&root).unwrap());
    assert_eq!(allocated, [root.as_os_str().len(), 48]);
    drop(entries);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn path_copies_are_exact_and_joins_stay_within_doubling() {
    let canonical = std::env::temp_dir().canonicalize().unwrap();
    assert_eq!(canonical.capacity(), canonical.as_os_str().len());
    let cloned = canonical.clone();
    assert_eq!(cloned.capacity(), cloned.as_os_str().len());
    let owned = canonical.as_path().to_path_buf();
    assert_eq!(owned.capacity(), owned.as_os_str().len());
    let joined = PathBuf::from("/r".repeat(20)).join("name.jsonl");
    assert_eq!(joined.as_os_str().len(), 51);
    assert_eq!(joined.capacity(), 80);
    for root_len in [1usize, 7, 8, 100, 1023, 4095] {
        let root = format!("/{}", "r".repeat(root_len - 1));
        for name_len in [1usize, 255, 1023] {
            let name = "n".repeat(name_len);
            let joined = Path::new(&root).join(&name);
            let len = joined.as_os_str().len();
            assert!(
                joined.capacity() <= (2 * len).max(8),
                "join of {root_len}+{name_len} grew to {} for {len} bytes",
                joined.capacity()
            );
        }
    }
    let base = canonical.as_path();
    let stem = OsStr::new("session-stem");
    let in_place = joined_in_place(base, stem);
    assert_eq!(in_place, base.join(stem).join("subagents"));
    assert_eq!(in_place.capacity(), in_place.as_os_str().len());
}

#[test]
fn hex_tokens_and_label_ids_land_on_their_lengths() {
    let token = format!("{:x}", Sha256::digest(b"t"));
    assert_eq!((token.len(), token.capacity()), (64, 64));
    let label = format!("labels:{token}");
    assert_eq!((label.len(), label.capacity()), (71, 71));
}

use std::alloc::{GlobalAlloc, Layout, System};
use std::mem::size_of;
use std::sync::atomic::{AtomicIsize, Ordering};

use cc_transcript_core::SLOTS_PER_ENTRY;
use imbl::{OrdMap, Vector};

type Key = [usize; 2];
type Value = [usize; 2];

const SLOT_OVERHEAD: usize = 4 * size_of::<usize>();
const MAP_WIDTH: usize = size_of::<Key>() + size_of::<Value>() + SLOT_OVERHEAD;
const VECTOR_WIDTH: usize = size_of::<Value>() + SLOT_OVERHEAD;
const LENGTHS: [usize; 5] = [64, 1_000, 5_000, 50_000, 200_000];

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live_bytes<R>(build: impl FnOnce() -> R) -> (R, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    let built = build();
    (
        built,
        (LIVE.load(Ordering::Relaxed) - before).max(0) as usize,
    )
}

fn scattered(index: usize) -> Key {
    [index.wrapping_mul(0x9E37_79B9_7F4A_7C15), index]
}

fn ascending(index: usize) -> Key {
    [index, index]
}

fn assert_fits(label: &str, len: usize, bytes: usize, root: usize, width: usize) {
    let charged = (len * SLOTS_PER_ENTRY + root) * width;
    eprintln!(
        "{label} len={len} live={bytes} charged={charged} slots_per_entry={:.2}",
        bytes as f64 / width as f64 / len as f64
    );
    assert!(bytes <= charged, "{label} len={len}: {bytes} > {charged}");
}

#[test]
fn an_ord_map_fits_its_charge_in_any_insertion_order() {
    for len in LENGTHS {
        for (label, key) in [
            ("ascending", ascending as fn(usize) -> Key),
            ("scattered", scattered as fn(usize) -> Key),
        ] {
            let (map, bytes) = live_bytes(|| {
                let mut map = OrdMap::new();
                for index in 0..len {
                    map.insert(key(index), [index; 2]);
                }
                map
            });
            assert_eq!(map.len(), len);
            assert_fits(label, len, bytes, 64, MAP_WIDTH);
        }
    }
}

#[test]
fn an_ord_map_thinned_by_removals_fits_its_charge() {
    for len in LENGTHS {
        let (map, bytes) = live_bytes(|| {
            let mut map = OrdMap::new();
            for index in 0..len * 2 {
                map.insert(scattered(index), [index; 2]);
            }
            for index in (0..len * 2).step_by(2) {
                map.remove(&scattered(index));
            }
            map
        });
        assert_eq!(map.len(), len);
        assert_fits("thinned", len, bytes, 64, MAP_WIDTH);
    }
}

#[test]
fn a_vector_fits_its_charge() {
    for len in LENGTHS {
        let (vector, bytes) = live_bytes(|| {
            let mut vector = Vector::new();
            for index in 0..len {
                vector.push_back([index; 2]);
            }
            vector
        });
        assert_eq!(vector.len(), len);
        assert_fits("vector", len, bytes, 256, VECTOR_WIDTH);
    }
}

#[test]
fn a_generation_appending_one_entry_copies_only_a_path() {
    for len in LENGTHS {
        let mut map = OrdMap::new();
        for index in 0..len {
            map.insert(scattered(index), [index; 2]);
        }
        let (next, bytes) = live_bytes(|| {
            let mut next = map.clone();
            next.insert(scattered(len), [len; 2]);
            next
        });
        assert_eq!(next.len(), len + 1);
        let path = ((len + 1).ilog2() as usize + 4) * 64 * MAP_WIDTH;
        eprintln!("path len={len} live={bytes} charged={path}");
        assert!(bytes <= path, "path len={len}: {bytes} > {path}");
    }
}

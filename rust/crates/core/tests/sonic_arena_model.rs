use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;

use cc_transcript_core::snapshot_memory::{arena_bytes, arena_requested_bytes};
use cc_transcript_core::value::normalize_last_wins;
use sonic_rs::{JsonContainerTrait, Value};

const TRACE_SLOTS: usize = 64;

struct Counting;

struct Record {
    armed: bool,
    live: isize,
    count: usize,
    sizes: [usize; TRACE_SLOTS],
}

impl Record {
    const fn new(armed: bool) -> Self {
        Self {
            armed,
            live: 0,
            count: 0,
            sizes: [0; TRACE_SLOTS],
        }
    }
}

thread_local! {
    static RECORD: RefCell<Record> = const { RefCell::new(Record::new(false)) };
}

fn note(delta: isize, size: Option<usize>) {
    let _ = RECORD.try_with(|record| {
        if let Ok(mut record) = record.try_borrow_mut() {
            if record.armed {
                record.live += delta;
                if let Some(size) = size {
                    let count = record.count;
                    if count < TRACE_SLOTS {
                        record.sizes[count] = size;
                    }
                    record.count += 1;
                }
            }
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size() as isize, Some(layout.size()));
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(-(layout.size() as isize), None);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size as isize - layout.size() as isize, Some(new_size));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measured<R>(op: impl FnOnce() -> R) -> (R, isize, Vec<usize>) {
    RECORD.with(|record| *record.borrow_mut() = Record::new(true));
    let result = op();
    let record =
        RECORD.with(|record| std::mem::replace(&mut *record.borrow_mut(), Record::new(false)));
    assert!(
        record.count <= TRACE_SLOTS,
        "{} allocations overflowed the trace",
        record.count
    );
    (result, record.live, record.sizes[..record.count].to_vec())
}

fn warm() {
    let wide = format!("[{}]", vec!["0"; 32768].join(","));
    drop(sonic_rs::from_slice::<Value>(wide.as_bytes()).unwrap());
}

fn parsed(doc: &str) -> (Value, isize, Vec<usize>) {
    warm();
    measured(|| sonic_rs::from_slice::<Value>(doc.as_bytes()).unwrap())
}

fn nodes(value: &Value) -> usize {
    if let Some(array) = value.as_array().filter(|array| !array.is_empty()) {
        16 * (array.len() + 1) + array.iter().map(nodes).sum::<usize>()
    } else if let Some(object) = value.as_object().filter(|object| !object.is_empty()) {
        16 * (2 * object.len() + 1) + object.iter().map(|(_, item)| nodes(item)).sum::<usize>()
    } else {
        0
    }
}

fn mirror(root: &Value, len: usize) -> usize {
    64 + len + 64 + (32 + nodes(root)).max(448) + 48
}

fn zeros(count: usize) -> String {
    format!("[{}]", vec!["0"; count].join(","))
}

#[test]
fn small_documents_allocate_exactly_the_modeled_arena() {
    let filled = format!(r#"{{"n":{}}}"#, zeros(22));
    for doc in [
        r#"{"a":1}"#,
        r#"{"type":"user","message":{"content":"hi"}}"#,
        "[]",
        filled.as_str(),
    ] {
        let (value, live, sizes) = parsed(doc);
        assert!(arena_requested_bytes(&value) <= 448, "{doc}");
        assert_eq!(arena_bytes(&value, doc.len()), mirror(&value, doc.len()));
        assert_eq!(live, mirror(&value, doc.len()) as isize, "{doc}: {sizes:?}");
        for size in [64, doc.len() + 64, 496] {
            assert!(
                sizes.contains(&size),
                "{doc}: {size} missing from {sizes:?}"
            );
        }
    }
    assert_eq!(
        arena_requested_bytes(&sonic_rs::from_str::<Value>(&filled).unwrap()),
        448
    );
}

#[test]
fn a_retained_subtree_pins_the_whole_arena_until_the_last_clone_drops() {
    let doc = r#"{"keep":{"a":[1,2,3]},"drop":"x"}"#;
    let (root, live, _) = parsed(doc);
    let charge = arena_bytes(&root, doc.len());
    assert_eq!(live, charge as isize);
    let (kept, cloned, sizes) = measured(|| root["keep"].clone());
    assert_eq!((cloned, sizes), (0, Vec::new()));
    let ((), released, sizes) = measured(|| drop(root));
    assert_eq!((released, sizes), (0, Vec::new()));
    let ((), freed, _) = measured(|| drop(kept));
    assert_eq!(freed, -(charge as isize));
}

#[test]
fn larger_documents_stay_within_the_declared_bump_residue() {
    let objects = format!(
        "[{}]",
        (0..512)
            .map(|index| format!(r#"{{"k{index}":[{index}]}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    let keys = format!(
        "{{{}}}",
        (0..2048)
            .map(|index| format!(r#""k{index}":{index}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    let nested = format!("{}0{}", "[".repeat(200), "]".repeat(200));
    let fragmenting = format!("[{}]", [1, 26, 33, 90, 161].map(zeros).join(","));
    for doc in [&zeros(4096), &objects, &keys, &nested, &fragmenting] {
        let (value, live, _) = parsed(doc);
        let modeled = arena_bytes(&value, doc.len());
        let requested = arena_requested_bytes(&value);
        assert_eq!(modeled, mirror(&value, doc.len()));
        assert_eq!(requested, 32 + nodes(&value));
        let live = usize::try_from(live).unwrap();
        assert!(live >= modeled, "{live} under the {modeled}-byte model");
        assert!(
            live <= modeled + 6 * requested + 4096,
            "{live} exceeds the {modeled}-byte model plus the declared residue for {requested} requested bytes"
        );
    }
    let (value, live, _) = parsed(&fragmenting);
    assert_eq!(arena_requested_bytes(&value), 5184);
    assert_eq!(
        usize::try_from(live).unwrap() - arena_bytes(&value, fragmenting.len()),
        496 + 1008 + 2032 + 4080 + 8176 - (5184 + 48)
    );
}

#[test]
fn duplicate_keys_reparse_into_a_private_arena_and_free_the_line() {
    let doc = r#"{"a":1,"a":2,"b":[1,2]}"#;
    let normalized = r#"{"a":2,"b":[1,2]}"#;
    warm();
    let (value, live, _) = measured(|| {
        let mut value = sonic_rs::from_slice::<Value>(doc.as_bytes()).unwrap();
        normalize_last_wins(&mut value);
        value
    });
    assert_eq!(sonic_rs::to_string(&value).unwrap(), normalized);
    assert_eq!(live, arena_bytes(&value, normalized.len()) as isize);
    assert_eq!(live, mirror(&value, normalized.len()) as isize);
}

#[test]
fn dom_containers_report_their_length_as_capacity() {
    let value = sonic_rs::from_slice::<Value>(br#"{"a":[1,2,3],"b":{"x":1,"y":2}}"#).unwrap();
    let root = value.as_object().unwrap();
    assert_eq!((root.len(), root.capacity()), (2, 2));
    let array = value["a"].as_array().unwrap();
    assert_eq!((array.len(), array.capacity()), (3, 3));
    let inner = value["b"].as_object().unwrap();
    assert_eq!((inner.len(), inner.capacity()), (2, 2));
}

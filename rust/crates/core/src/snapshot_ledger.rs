use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::mem::{replace, size_of};
use std::ops::{Deref, DerefMut, Index};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use crate::snapshot_memory::MemoryCharge;

#[cfg(all(
    target_feature = "sse2",
    any(target_arch = "x86", target_arch = "x86_64")
))]
const GROUP_WIDTH: usize = 16;
#[cfg(all(
    target_arch = "aarch64",
    target_feature = "neon",
    target_endian = "little"
))]
const GROUP_WIDTH: usize = 8;
#[cfg(not(any(
    all(
        target_feature = "sse2",
        any(target_arch = "x86", target_arch = "x86_64")
    ),
    all(
        target_arch = "aarch64",
        target_feature = "neon",
        target_endian = "little"
    )
)))]
const GROUP_WIDTH: usize = size_of::<usize>();

fn bucket_mask_to_capacity(bucket_mask: usize) -> usize {
    if bucket_mask < 8 {
        bucket_mask
    } else {
        ((bucket_mask + 1) / 8) * 7
    }
}

fn capacity_to_buckets(cap: usize, element_size: usize) -> usize {
    if cap < 15 {
        let min_cap = match (GROUP_WIDTH, element_size) {
            (16, 0..=1) => 14,
            (16, 2..=3) | (8, 0..=1) => 7,
            _ => 3,
        };
        return match min_cap.max(cap) {
            0..=3 => 4,
            4..=7 => 8,
            _ => 16,
        };
    }
    (cap.checked_mul(8).expect("hashbrown capacity") / 7).next_power_of_two()
}

pub(crate) fn hashbrown_tier(cap: usize, element_size: usize) -> usize {
    bucket_mask_to_capacity(capacity_to_buckets(cap, element_size) - 1)
}

fn is_hashbrown_tier(reserved: usize) -> bool {
    reserved == 0
        || (2..usize::BITS)
            .map(|bits| bucket_mask_to_capacity((1usize << bits) - 1))
            .any(|tier| tier == reserved)
}

fn min_non_zero_cap(element_size: usize) -> usize {
    match element_size {
        1 => 8,
        2..=1024 => 4,
        _ => 1,
    }
}

pub(crate) fn vec_capacity_after(
    cap: usize,
    len: usize,
    additional: usize,
    element_size: usize,
) -> usize {
    if additional <= cap - len {
        cap
    } else {
        (cap * 2)
            .max(len + additional)
            .max(min_non_zero_cap(element_size))
    }
}

pub(crate) fn vec_capacity_for<T>(vec: &Vec<T>, additional: usize) -> usize {
    vec_capacity_after(vec.capacity(), vec.len(), additional, size_of::<T>())
}

pub(crate) fn set_capacity_for<T>(set: &HashSet<T>, additional: usize) -> usize {
    if additional <= set.capacity() - set.len() {
        return set.capacity();
    }
    hashbrown_tier(
        (set.len() + additional).max(set.capacity() + 1),
        size_of::<T>(),
    )
}

pub(crate) fn vec_growth<T>(vec: &Vec<T>, additional: usize) -> usize {
    (vec_capacity_for(vec, additional) - vec.capacity()) * size_of::<T>()
}

pub(crate) fn set_growth<T>(set: &HashSet<T>, additional: usize) -> usize {
    (set_capacity_for(set, additional) - set.capacity()) * size_of::<T>()
}

pub(crate) fn deque_capacity_for<T>(deque: &VecDeque<T>, additional: usize) -> usize {
    vec_capacity_after(deque.capacity(), deque.len(), additional, size_of::<T>())
}

pub(crate) fn deque_growth<T>(deque: &VecDeque<T>, additional: usize) -> usize {
    (deque_capacity_for(deque, additional) - deque.capacity()) * size_of::<T>()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerEvent {
    Generation(usize),
    Chunk(usize),
}

#[derive(Default)]
pub struct ReleaseQueue {
    events: Mutex<Vec<LedgerEvent>>,
}

impl ReleaseQueue {
    fn push(&self, event: LedgerEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    pub(crate) fn take(&self) -> Vec<LedgerEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn buffer_bytes(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .capacity()
            * size_of::<LedgerEvent>()
    }
}

#[derive(Debug, Default)]
pub struct LedgerHook(OnceLock<(Weak<ReleaseQueue>, LedgerEvent)>);

impl LedgerHook {
    pub(crate) fn arm(&self, queue: &Arc<ReleaseQueue>, event: LedgerEvent) {
        let (armed_queue, armed_event) = self.0.get_or_init(|| (Arc::downgrade(queue), event));
        assert!(
            *armed_event == event && Weak::as_ptr(armed_queue) == Arc::as_ptr(queue),
            "one snapshot, one store"
        );
    }
}

impl Drop for LedgerHook {
    fn drop(&mut self) {
        if let Some((queue, event)) = self.0.get() {
            if let Some(queue) = queue.upgrade() {
                queue.push(*event);
            }
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trace {
    Admitted(usize),
    Allocated(usize),
    Reserved(usize),
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Work {
    ticks: Arc<AtomicUsize>,
    reclaims: Arc<AtomicUsize>,
    trace: Arc<Mutex<Vec<Trace>>>,
}

#[cfg(not(test))]
#[derive(Clone, Copy, Default)]
pub(crate) struct Work;

#[cfg(test)]
impl Work {
    pub(crate) fn tick(&self, units: usize) {
        self.ticks.fetch_add(units, Ordering::Relaxed);
    }

    pub(crate) fn reclaim(&self, units: usize) {
        self.reclaims.fetch_add(units, Ordering::Relaxed);
    }

    pub(crate) fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.ticks)
    }

    pub(crate) fn reclaims(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.reclaims)
    }

    pub(crate) fn admitted(&self, bytes: usize) {
        self.trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Trace::Admitted(bytes));
    }

    pub(crate) fn allocated(&self, tier: usize) {
        self.trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Trace::Allocated(tier));
    }

    pub(crate) fn reserved(&self, bytes: usize) {
        self.trace
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Trace::Reserved(bytes));
    }

    pub(crate) fn traced(&self) -> Vec<Trace> {
        std::mem::take(&mut *self.trace.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(not(test))]
impl Work {
    pub(crate) fn tick(&self, _units: usize) {}

    pub(crate) fn reclaim(&self, _units: usize) {}

    pub(crate) fn admitted(&self, _bytes: usize) {}

    pub(crate) fn allocated(&self, _tier: usize) {}

    pub(crate) fn reserved(&self, _bytes: usize) {}
}

pub(crate) trait Reserved {
    fn reserved(&self) -> usize;

    #[cfg(test)]
    fn audit_reserved(&self);
}

pub(crate) struct Table<K, V> {
    map: HashMap<K, V>,
    reserved: usize,
    work: Work,
}

impl<K: Eq + Hash, V> Table<K, V> {
    pub(crate) fn new(work: Work) -> Self {
        Self {
            map: HashMap::new(),
            reserved: 0,
            work,
        }
    }

    pub(crate) fn reserved_bytes(&self) -> usize {
        self.reserved * size_of::<(K, V)>()
    }

    fn tier_after(&self, additional: usize) -> usize {
        if additional <= self.map.capacity() - self.map.len() {
            return self.reserved;
        }
        let items = self.map.len() + additional;
        if items <= self.reserved / 2 {
            return self.reserved;
        }
        hashbrown_tier(items.max(self.reserved + 1), size_of::<(K, V)>())
    }

    pub(crate) fn growth(&self, additional: usize) -> usize {
        (self.tier_after(additional) - self.reserved) * size_of::<(K, V)>()
    }

    pub(crate) fn growth_for<Q>(&self, key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.growth(usize::from(!self.map.contains_key(key)))
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        let predicted = self.tier_after(additional);
        self.map.reserve(additional);
        self.settle(predicted);
    }

    pub(crate) fn reserve_for<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.reserve(usize::from(!self.map.contains_key(key)));
    }

    fn settle(&mut self, predicted: usize) {
        let before = self.reserved;
        self.reserved = self.reserved.max(self.map.capacity());
        assert_eq!(
            self.reserved, predicted,
            "retained table landed off its predicted tier"
        );
        if self.reserved != before {
            self.work.allocated(self.reserved);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.map.capacity()
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.contains_key(key)
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.get(key)
    }

    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.get_mut(key)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let predicted = self.tier_after(1);
        let displaced = match self.map.entry(key) {
            Entry::Occupied(mut occupied) => Some(replace(occupied.get_mut(), value)),
            Entry::Vacant(vacant) => {
                vacant.insert(value);
                None
            }
        };
        if displaced.is_none() {
            self.settle(predicted);
        }
        displaced
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.remove(key)
    }

    pub(crate) fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.remove_entry(key)
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&K, &mut V) -> bool) {
        self.map.retain(keep);
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.map.iter()
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.map.keys()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values()
    }
}

impl<K: Eq + Hash, V> Reserved for Table<K, V> {
    fn reserved(&self) -> usize {
        self.reserved
    }

    #[cfg(test)]
    fn audit_reserved(&self) {
        assert!(
            self.reserved >= self.map.capacity(),
            "retained table reservation trails its capacity"
        );
        assert!(
            self.reserved >= self.map.len(),
            "retained table reservation trails its length"
        );
        assert!(
            is_hashbrown_tier(self.reserved),
            "retained table reservation {} is not a hashbrown tier",
            self.reserved
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorKind {
    Entries,
    Indexes,
    Facts,
    Warm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) id: usize,
    pub(crate) bytes: usize,
    pub(crate) kind: AnchorKind,
}

impl Anchor {
    pub(crate) fn entries(id: usize, charge: MemoryCharge) -> Self {
        Self {
            id,
            bytes: charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes,
            kind: AnchorKind::Entries,
        }
    }

    pub(crate) fn indexes((id, bytes): (usize, usize)) -> Self {
        Self {
            id,
            bytes,
            kind: AnchorKind::Indexes,
        }
    }

    pub(crate) fn facts(id: usize, bytes: usize) -> Self {
        Self {
            id,
            bytes,
            kind: AnchorKind::Facts,
        }
    }

    pub(crate) fn warm(id: usize, bytes: usize) -> Self {
        Self {
            id,
            bytes,
            kind: AnchorKind::Warm,
        }
    }
}

struct Shared {
    owners: u32,
    bytes: usize,
    kind: AnchorKind,
}

pub(crate) struct SharedAllocations {
    owners: Table<usize, Shared>,
    entries: usize,
    indexes: usize,
    facts: usize,
    warm: usize,
    work: Work,
}

impl SharedAllocations {
    fn new(work: Work) -> Self {
        Self {
            owners: Table::new(work.clone()),
            entries: 0,
            indexes: 0,
            facts: 0,
            warm: 0,
            work,
        }
    }

    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    pub(crate) fn indexes(&self) -> usize {
        self.indexes
    }

    pub(crate) fn facts(&self) -> usize {
        self.facts
    }

    pub(crate) fn warm(&self) -> usize {
        self.warm
    }

    pub(crate) fn table_bytes(&self) -> usize {
        self.owners.reserved_bytes()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.owners.len()
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.owners.capacity()
    }

    pub(crate) fn work(&self) -> &Work {
        &self.work
    }

    fn total(&mut self, kind: AnchorKind) -> &mut usize {
        match kind {
            AnchorKind::Entries => &mut self.entries,
            AnchorKind::Indexes => &mut self.indexes,
            AnchorKind::Facts => &mut self.facts,
            AnchorKind::Warm => &mut self.warm,
        }
    }

    pub(crate) fn acquire(&mut self, anchor: Anchor) {
        self.work.tick(1);
        if let Some(existing) = self.owners.get_mut(&anchor.id) {
            debug_assert_eq!(
                (existing.bytes, existing.kind),
                (anchor.bytes, anchor.kind),
                "retained allocation re-acquired with a different charge"
            );
            existing.owners += 1;
            return;
        }
        self.owners.insert(
            anchor.id,
            Shared {
                owners: 1,
                bytes: anchor.bytes,
                kind: anchor.kind,
            },
        );
        *self.total(anchor.kind) += anchor.bytes;
    }

    pub(crate) fn release(&mut self, id: usize) {
        self.work.tick(1);
        let existing = self.owners.get_mut(&id).expect("balanced retained ledger");
        existing.owners -= 1;
        if existing.owners > 0 {
            return;
        }
        let Shared { bytes, kind, .. } = self.owners.remove(&id).expect("balanced retained ledger");
        let total = self.total(kind);
        *total = total.checked_sub(bytes).expect("balanced retained ledger");
    }

    pub(crate) fn unowned(&self, anchors: impl IntoIterator<Item = Anchor>) -> (usize, usize) {
        let mut seen = HashSet::new();
        anchors
            .into_iter()
            .filter(|anchor| {
                self.work.tick(1);
                !self.owners.contains_key(&anchor.id) && seen.insert(anchor.id)
            })
            .fold((0, 0), |(bytes, additional), anchor| {
                (bytes + anchor.bytes, additional + 1)
            })
    }

    pub(crate) fn unowned_bytes(&self, anchors: impl IntoIterator<Item = Anchor>) -> usize {
        self.unowned(anchors).0
    }

    pub(crate) fn growth(&self, anchors: impl IntoIterator<Item = Anchor>) -> usize {
        self.owners.growth(self.unowned(anchors).1)
    }

    pub(crate) fn admission(&self, anchors: impl IntoIterator<Item = Anchor>) -> usize {
        let (bytes, additional) = self.unowned(anchors);
        bytes + self.owners.growth(additional)
    }

    pub(crate) fn reserve(&mut self, anchors: impl IntoIterator<Item = Anchor>) {
        let (_, additional) = self.unowned(anchors);
        self.owners.reserve(additional);
    }
}

impl Reserved for SharedAllocations {
    fn reserved(&self) -> usize {
        self.owners.reserved()
    }

    #[cfg(test)]
    fn audit_reserved(&self) {
        self.owners.audit_reserved();
    }
}

pub(crate) struct RetainedLedger {
    pub(crate) shared: SharedAllocations,
    pub(crate) queue: Arc<ReleaseQueue>,
    pub(crate) pending: usize,
    pub(crate) classifier: usize,
}

impl RetainedLedger {
    pub(crate) fn new(work: Work) -> Self {
        Self {
            shared: SharedAllocations::new(work),
            queue: Arc::new(ReleaseQueue::default()),
            pending: 0,
            classifier: 0,
        }
    }
}

pub(crate) trait TicketKey {
    fn owned_bytes(&self) -> usize;
}

impl TicketKey for String {
    fn owned_bytes(&self) -> usize {
        self.capacity()
    }
}

struct Ticket<K> {
    deadline: u64,
    key: K,
}

impl<K> PartialEq for Ticket<K> {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl<K> Eq for Ticket<K> {}

impl<K> PartialOrd for Ticket<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Ticket<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

pub(crate) struct ExpiryIndex<K> {
    heap: BinaryHeap<Ticket<K>>,
    key_bytes: usize,
    work: Work,
}

impl<K: TicketKey> ExpiryIndex<K> {
    pub(crate) fn new(work: Work) -> Self {
        Self {
            heap: BinaryHeap::new(),
            key_bytes: 0,
            work,
        }
    }

    pub(crate) fn push(&mut self, deadline: u64, key: K) {
        self.work.tick(1);
        let before = self.heap.capacity();
        let predicted = self.capacity_after(1);
        self.key_bytes += key.owned_bytes();
        self.heap.push(Ticket { deadline, key });
        self.settle(before, predicted);
    }

    pub(crate) fn growth(&self, additional: usize) -> usize {
        (self.capacity_after(additional) - self.heap.capacity()) * size_of::<Ticket<K>>()
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        let before = self.heap.capacity();
        let predicted = self.capacity_after(additional);
        self.heap.reserve(additional);
        self.settle(before, predicted);
    }

    fn capacity_after(&self, additional: usize) -> usize {
        vec_capacity_after(
            self.heap.capacity(),
            self.heap.len(),
            additional,
            size_of::<Ticket<K>>(),
        )
    }

    fn settle(&self, before: usize, predicted: usize) {
        assert_eq!(
            self.heap.capacity(),
            predicted,
            "expiry index landed off its predicted capacity"
        );
        if predicted != before {
            self.work.allocated(predicted);
        }
    }

    fn pop(&mut self) -> Option<Ticket<K>> {
        let ticket = self.heap.pop()?;
        self.work.tick(1);
        self.key_bytes = self
            .key_bytes
            .checked_sub(ticket.key.owned_bytes())
            .expect("balanced expiry index");
        Some(ticket)
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.heap.capacity() * size_of::<Ticket<K>>() + self.key_bytes
    }

    #[cfg(test)]
    pub(crate) fn key_bytes(&self) -> usize {
        self.key_bytes
    }

    #[cfg(test)]
    pub(crate) fn audit_key_bytes(&self) -> usize {
        self.summed_key_bytes()
    }

    fn summed_key_bytes(&self) -> usize {
        self.heap
            .iter()
            .map(|ticket| ticket.key.owned_bytes())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    pub(crate) fn crowded(&self, live: usize) -> bool {
        self.heap.len() > 2 * live
    }

    pub(crate) fn rebuild(&mut self, live: usize, tickets: impl IntoIterator<Item = (u64, K)>) {
        let mut rebuilt = Vec::with_capacity(live);
        rebuilt.extend(
            tickets
                .into_iter()
                .map(|(deadline, key)| Ticket { deadline, key }),
        );
        assert!(
            rebuilt.capacity() <= self.heap.capacity(),
            "expiry index rebuild grew the heap"
        );
        self.heap = BinaryHeap::from(rebuilt);
        self.key_bytes = self.summed_key_bytes();
        self.work.tick(self.heap.len());
    }

    pub(crate) fn expired(
        &mut self,
        now: u64,
        mut deadline_of: impl FnMut(&K) -> Option<u64>,
    ) -> Vec<K> {
        let mut expired = Vec::new();
        while self
            .heap
            .peek()
            .is_some_and(|ticket| ticket.deadline <= now)
        {
            let Ticket { key, .. } = self.pop().expect("peeked ticket");
            match deadline_of(&key) {
                None => {}
                Some(current) if current <= now => {
                    self.work.reclaim(1);
                    expired.push(key);
                }
                Some(current) => self.push(current, key),
            }
        }
        expired
    }

    pub(crate) fn pop_earliest(
        &mut self,
        mut deadline_of: impl FnMut(&K) -> Option<u64>,
    ) -> Option<K> {
        while let Some(Ticket { deadline, key }) = self.pop() {
            match deadline_of(&key) {
                None => {}
                Some(current) if current == deadline => return Some(key),
                Some(current) => self.push(current, key),
            }
        }
        None
    }
}

impl<K> Reserved for ExpiryIndex<K> {
    fn reserved(&self) -> usize {
        self.heap.capacity()
    }

    #[cfg(test)]
    fn audit_reserved(&self) {}
}

pub(crate) struct DeadlineIndex<K> {
    deadlines: BTreeSet<(u64, K)>,
    work: Work,
}

impl<K: Ord> DeadlineIndex<K> {
    pub(crate) fn new(work: Work) -> Self {
        Self {
            deadlines: BTreeSet::new(),
            work,
        }
    }

    pub(crate) fn insert(&mut self, deadline: u64, key: K) {
        self.work.tick(1);
        assert!(
            self.deadlines.insert((deadline, key)),
            "one deadline per indexed key"
        );
    }

    pub(crate) fn remove(&mut self, deadline: u64, key: K) {
        self.work.tick(1);
        assert!(
            self.deadlines.remove(&(deadline, key)),
            "balanced deadline index"
        );
    }

    pub(crate) fn earliest(&self) -> Option<&K> {
        self.work.tick(1);
        self.deadlines.first().map(|(_, key)| key)
    }

    pub(crate) fn pop_expired(&mut self, now: u64) -> Option<K> {
        self.work.tick(1);
        if !self
            .deadlines
            .first()
            .is_some_and(|(deadline, _)| *deadline <= now)
        {
            return None;
        }
        self.work.reclaim(1);
        self.deadlines.pop_first().map(|(_, key)| key)
    }

    pub(crate) fn len(&self) -> usize {
        self.deadlines.len()
    }

    pub(crate) fn index_bytes(&self) -> usize {
        self.deadlines.len() * size_of::<(u64, K)>()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, deadline: u64, key: &K) -> bool
    where
        K: Clone,
    {
        self.deadlines.contains(&(deadline, key.clone()))
    }
}

pub(crate) trait Charge<K> {
    fn key_charge(_key: &K) -> usize {
        0
    }

    fn charge(&self) -> usize;
}

pub(crate) fn charged_bytes<K, V: Charge<K>>(key: &K, value: &V) -> usize {
    V::key_charge(key) + value.charge()
}

struct Slot<V> {
    value: V,
    key_charge: usize,
    charge: usize,
    pledge: usize,
}

pub(crate) struct Ledgered<K, V> {
    map: Table<K, Slot<V>>,
    charged: usize,
    work: Work,
}

pub(crate) struct ChargedMut<'a, V> {
    slot: &'a mut Slot<V>,
    charged: &'a mut usize,
    charge: fn(&V) -> usize,
    work: &'a Work,
}

impl<V> Deref for ChargedMut<'_, V> {
    type Target = V;

    fn deref(&self) -> &V {
        &self.slot.value
    }
}

impl<V> DerefMut for ChargedMut<'_, V> {
    fn deref_mut(&mut self) -> &mut V {
        &mut self.slot.value
    }
}

impl<V> Drop for ChargedMut<'_, V> {
    fn drop(&mut self) {
        let charge = (self.charge)(&self.slot.value);
        *self.charged = self
            .charged
            .checked_sub(self.slot.charge)
            .expect("balanced retained ledger")
            + charge;
        self.slot.charge = charge;
        self.work.tick(1);
    }
}

impl<K: Eq + Hash, V: Charge<K>> Ledgered<K, V> {
    pub(crate) fn new(work: Work) -> Self {
        Self {
            map: Table::new(work.clone()),
            charged: 0,
            work,
        }
    }

    pub(crate) fn charged(&self) -> usize {
        self.charged
    }

    pub(crate) fn capacity_bytes(&self) -> usize {
        self.map.reserved_bytes()
    }

    pub(crate) fn growth(&self, additional: usize) -> usize {
        self.map.growth(additional)
    }

    pub(crate) fn growth_for<Q>(&self, key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.growth_for(key)
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.map.reserve(additional);
    }

    pub(crate) fn reserve_for<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.reserve_for(key);
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.contains_key(key)
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.get(key).map(|slot| &slot.value)
    }

    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<ChargedMut<'_, V>>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let slot = self.map.get_mut(key)?;
        Some(ChargedMut {
            slot,
            charged: &mut self.charged,
            charge: V::charge,
            work: &self.work,
        })
    }

    pub(crate) fn pledge<Q>(&mut self, key: &Q, bytes: usize)
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let slot = self.map.get_mut(key).expect("pledged entry");
        slot.pledge += bytes;
        self.charged += bytes;
    }

    pub(crate) fn consume_pledge<Q>(&mut self, key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        let Some(slot) = self.map.get_mut(key) else {
            return 0;
        };
        let pledge = replace(&mut slot.pledge, 0);
        self.charged = self
            .charged
            .checked_sub(pledge)
            .expect("balanced retained ledger");
        pledge
    }

    #[cfg(test)]
    pub(crate) fn pledged<Q>(&self, key: &Q) -> usize
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.map.get(key).map_or(0, |slot| slot.pledge)
    }

    #[cfg(test)]
    pub(crate) fn entry_bytes() -> usize {
        size_of::<(K, Slot<V>)>()
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.work.tick(1);
        let charge = value.charge();
        if let Some(slot) = self.map.get_mut(&key) {
            self.charged = self
                .charged
                .checked_sub(slot.charge)
                .expect("balanced retained ledger")
                + charge;
            slot.charge = charge;
            return Some(replace(&mut slot.value, value));
        }
        let key_charge = V::key_charge(&key);
        self.charged += key_charge + charge;
        self.map.insert(
            key,
            Slot {
                value,
                key_charge,
                charge,
                pledge: 0,
            },
        );
        None
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.remove_entry(key).map(|(_, value)| value)
    }

    pub(crate) fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.work.tick(1);
        let (key, slot) = self.map.remove_entry(key)?;
        self.charged = self
            .charged
            .checked_sub(slot.key_charge + slot.charge + slot.pledge)
            .expect("balanced retained ledger");
        Some((key, slot.value))
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let charged = &mut self.charged;
        let work = &self.work;
        self.map.retain(|key, slot| {
            work.tick(1);
            let kept = keep(key, &slot.value);
            if !kept {
                work.reclaim(1);
                *charged = charged
                    .checked_sub(slot.key_charge + slot.charge + slot.pledge)
                    .expect("balanced retained ledger");
            }
            kept
        });
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.work.tick(self.map.len());
        self.map.clear();
        self.charged = 0;
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.map.iter().map(|(key, slot)| (key, &slot.value))
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values().map(|slot| &slot.value)
    }

    #[cfg(test)]
    pub(crate) fn audit_with(&self, bytes: impl Fn(&K, &V) -> usize) -> usize {
        self.map
            .iter()
            .map(|(key, slot)| bytes(key, &slot.value) + slot.pledge)
            .sum()
    }
}

impl<K: Eq + Hash, V: Charge<K>> Reserved for Ledgered<K, V> {
    fn reserved(&self) -> usize {
        self.map.reserved()
    }

    #[cfg(test)]
    fn audit_reserved(&self) {
        self.map.audit_reserved();
    }
}

impl<K, V, Q> Index<&Q> for Ledgered<K, V>
where
    K: Eq + Hash + Borrow<Q>,
    V: Charge<K>,
    Q: ?Sized + Hash + Eq,
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("ledgered entry")
    }
}

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::mem::size_of;
use std::ops::{Deref, DerefMut, Index};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use crate::snapshot_memory::MemoryCharge;

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
#[derive(Clone, Default)]
pub(crate) struct Work(Arc<AtomicUsize>);

#[cfg(not(test))]
#[derive(Clone, Copy, Default)]
pub(crate) struct Work;

#[cfg(test)]
impl Work {
    pub(crate) fn tick(&self, units: usize) {
        self.0.fetch_add(units, Ordering::Relaxed);
    }

    pub(crate) fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.0)
    }
}

#[cfg(not(test))]
impl Work {
    pub(crate) fn tick(&self, _units: usize) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnchorKind {
    Entries,
    Indexes,
    Facts,
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
}

struct Shared {
    owners: u32,
    bytes: usize,
    kind: AnchorKind,
}

pub(crate) struct SharedAllocations {
    owners: HashMap<usize, Shared>,
    entries: usize,
    indexes: usize,
    facts: usize,
    work: Work,
}

impl SharedAllocations {
    fn new(work: Work) -> Self {
        Self {
            owners: HashMap::new(),
            entries: 0,
            indexes: 0,
            facts: 0,
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

    pub(crate) fn table_bytes(&self) -> usize {
        self.owners.capacity() * size_of::<(usize, Shared)>()
    }

    pub(crate) fn work(&self) -> &Work {
        &self.work
    }

    fn total(&mut self, kind: AnchorKind) -> &mut usize {
        match kind {
            AnchorKind::Entries => &mut self.entries,
            AnchorKind::Indexes => &mut self.indexes,
            AnchorKind::Facts => &mut self.facts,
        }
    }

    pub(crate) fn acquire(&mut self, anchor: Anchor) {
        self.work.tick(1);
        let charged = match self.owners.entry(anchor.id) {
            Entry::Occupied(mut existing) => {
                let existing = existing.get_mut();
                debug_assert_eq!(
                    (existing.bytes, existing.kind),
                    (anchor.bytes, anchor.kind),
                    "retained allocation re-acquired with a different charge"
                );
                existing.owners += 1;
                false
            }
            Entry::Vacant(vacant) => {
                vacant.insert(Shared {
                    owners: 1,
                    bytes: anchor.bytes,
                    kind: anchor.kind,
                });
                true
            }
        };
        if charged {
            *self.total(anchor.kind) += anchor.bytes;
        }
    }

    pub(crate) fn release(&mut self, id: usize) {
        self.work.tick(1);
        let Entry::Occupied(mut existing) = self.owners.entry(id) else {
            panic!("balanced retained ledger");
        };
        existing.get_mut().owners -= 1;
        if existing.get().owners > 0 {
            return;
        }
        let Shared { bytes, kind, .. } = existing.remove();
        let total = self.total(kind);
        *total = total.checked_sub(bytes).expect("balanced retained ledger");
    }

    pub(crate) fn unowned_bytes(&self, anchors: impl IntoIterator<Item = Anchor>) -> usize {
        let mut seen = HashSet::new();
        anchors
            .into_iter()
            .filter(|anchor| {
                self.work.tick(1);
                !self.owners.contains_key(&anchor.id) && seen.insert(anchor.id)
            })
            .map(|anchor| anchor.bytes)
            .sum()
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

    pub(crate) fn storage_bytes(&self) -> usize {
        self.shared.table_bytes() + self.queue.buffer_bytes()
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
}

pub(crate) struct Ledgered<K, V> {
    map: HashMap<K, Slot<V>>,
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
            map: HashMap::new(),
            charged: 0,
            work,
        }
    }

    pub(crate) fn charged(&self) -> usize {
        self.charged
    }

    pub(crate) fn capacity_bytes(&self) -> usize {
        self.map.capacity() * size_of::<(K, Slot<V>)>()
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

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.work.tick(1);
        let key_charge = V::key_charge(&key);
        let charge = value.charge();
        self.charged += key_charge + charge;
        let displaced = self.map.insert(
            key,
            Slot {
                value,
                key_charge,
                charge,
            },
        )?;
        self.charged = self
            .charged
            .checked_sub(displaced.key_charge + displaced.charge)
            .expect("balanced retained ledger");
        Some(displaced.value)
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Hash + Eq,
    {
        self.work.tick(1);
        let slot = self.map.remove(key)?;
        self.charged = self
            .charged
            .checked_sub(slot.key_charge + slot.charge)
            .expect("balanced retained ledger");
        Some(slot.value)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let charged = &mut self.charged;
        let work = &self.work;
        self.map.retain(|key, slot| {
            work.tick(1);
            let kept = keep(key, &slot.value);
            if !kept {
                *charged = charged
                    .checked_sub(slot.key_charge + slot.charge)
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
    pub(crate) fn audit_charged(&self) -> usize {
        self.map
            .iter()
            .map(|(key, slot)| V::key_charge(key) + slot.value.charge())
            .sum()
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

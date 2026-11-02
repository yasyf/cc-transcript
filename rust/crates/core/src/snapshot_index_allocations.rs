use std::collections::HashMap;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::snapshot_ledger::{
    arc_bytes, LedgerEvent, LedgerHook, ReleaseQueue, MUTEX_STORAGE_BYTES,
};

#[derive(Debug)]
struct RegistryOwner {
    owner: Weak<AllocationOwner>,
    pending: Option<usize>,
}

#[derive(Debug, Default)]
struct RegistryState {
    queue: Vec<(usize, Weak<AllocationOwner>)>,
    owners: HashMap<usize, RegistryOwner>,
    owners_capacity: usize,
}

impl RegistryState {
    fn unqueue(&mut self, id: usize) {
        if let Some(at) = self
            .owners
            .get_mut(&id)
            .and_then(|owner| owner.pending.take())
        {
            let removed = self.queue.swap_remove(at);
            assert_eq!(removed.0, id, "pending owner position");
            if let Some((moved, _)) = self.queue.get(at) {
                self.owners.get_mut(moved).expect("queued owner").pending = Some(at);
            }
        }
    }
}

#[derive(Debug, Default)]
struct Registry {
    state: Mutex<RegistryState>,
    pending_bytes: AtomicUsize,
    admitted_bytes: AtomicUsize,
    hook: LedgerHook,
}

#[derive(Debug, Clone)]
pub(crate) struct RegistryWitness(Weak<Registry>);

impl RegistryWitness {
    pub(crate) fn allocation(&self) -> Option<(usize, usize)> {
        self.0.upgrade().map(|registry| registry.allocation())
    }

    pub(crate) fn credit_admitted(&self, bytes: usize) {
        if let Some(registry) = self.0.upgrade() {
            registry.admitted_bytes.store(bytes, Ordering::Release);
        }
    }
}

impl Registry {
    fn allocation(self: &Arc<Self>) -> (usize, usize) {
        let state = self.state.lock().expect("allocation registry");
        let bytes = (arc_bytes::<Registry>() + MUTEX_STORAGE_BYTES)
            .saturating_add(
                state
                    .queue
                    .capacity()
                    .saturating_mul(size_of::<(usize, Weak<AllocationOwner>)>()),
            )
            .saturating_add(
                state
                    .owners_capacity
                    .saturating_mul(4 * (size_of::<(usize, RegistryOwner)>() + size_of::<usize>())),
            )
            .saturating_add(
                state
                    .queue
                    .len()
                    .saturating_add(state.owners.len())
                    .saturating_mul(arc_bytes::<AllocationOwner>()),
            );
        (Arc::as_ptr(self) as usize, bytes)
    }
}

#[derive(Debug)]
pub(crate) struct AllocationOwner {
    pub(crate) id: usize,
    pub(crate) bytes: usize,
    hook: LedgerHook,
    pending: AtomicBool,
    registry: Arc<Registry>,
}

impl AllocationOwner {
    pub(crate) fn registry_id(&self) -> usize {
        Arc::as_ptr(&self.registry) as usize
    }

    pub(crate) fn arm(&self, queue: &Arc<ReleaseQueue>, event: LedgerEvent) -> bool {
        self.hook.arm(queue, event);
        let fresh = self.pending.swap(false, Ordering::AcqRel);
        if fresh {
            self.registry
                .pending_bytes
                .fetch_sub(self.bytes, Ordering::AcqRel);
            let mut state = self.registry.state.lock().expect("allocation registry");
            assert!(state
                .owners
                .get(&self.id)
                .is_some_and(|owner| std::ptr::eq(Weak::as_ptr(&owner.owner), self)));
            state.unqueue(self.id);
        }
        fresh
    }
}

impl Drop for AllocationOwner {
    fn drop(&mut self) {
        if self.pending.swap(false, Ordering::AcqRel) {
            self.registry
                .pending_bytes
                .fetch_sub(self.bytes, Ordering::AcqRel);
        }
        let mut state = self.registry.state.lock().expect("allocation registry");
        if state
            .owners
            .get(&self.id)
            .is_some_and(|owner| std::ptr::eq(Weak::as_ptr(&owner.owner), self))
        {
            state.unqueue(self.id);
            state.owners.remove(&self.id);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AllocationSet {
    entries: imbl::OrdMap<usize, Arc<AllocationOwner>>,
    bytes: usize,
    metadata: Arc<()>,
    registry: Arc<Registry>,
}

impl Default for AllocationSet {
    fn default() -> Self {
        Self {
            entries: imbl::OrdMap::new(),
            bytes: 0,
            metadata: Arc::new(()),
            registry: Arc::default(),
        }
    }
}

impl AllocationSet {
    pub(crate) const EMPTY_ALLOCATION_BYTES: usize =
        arc_bytes::<()>() + (arc_bytes::<Registry>() + MUTEX_STORAGE_BYTES);

    pub(crate) fn registry_witness(&self) -> RegistryWitness {
        RegistryWitness(Arc::downgrade(&self.registry))
    }

    pub(crate) fn cold_reservation(count: usize) -> usize {
        let width = size_of::<usize>() + size_of::<Arc<AllocationOwner>>() + 4 * size_of::<usize>();
        let membership = if count == 0 {
            0
        } else {
            (count.saturating_mul(8) + 64)
                .saturating_mul(width)
                .saturating_add((count.ilog2() as usize + 4).saturating_mul(64 * width))
        };
        let registry = (arc_bytes::<Registry>() + MUTEX_STORAGE_BYTES)
            .saturating_add(
                count
                    .saturating_mul(8 * (size_of::<(usize, RegistryOwner)>() + size_of::<usize>())),
            )
            .saturating_add(count.saturating_mul(2 * size_of::<(usize, Weak<AllocationOwner>)>()))
            .saturating_add(count.saturating_mul(2 * arc_bytes::<AllocationOwner>()));
        let pending = count.saturating_mul(2 * size_of::<Arc<AllocationOwner>>());
        membership
            .saturating_add(registry)
            .saturating_add(pending)
            .saturating_add(arc_bytes::<()>())
    }

    pub(crate) fn insert(&mut self, id: usize, bytes: usize) {
        if let Some(owner) = self.entries.get(&id) {
            assert_eq!(owner.bytes, bytes, "one allocation, one charge");
            return;
        }
        let owner = {
            let mut state = self.registry.state.lock().expect("allocation registry");
            if let Some(owner) = state
                .owners
                .get(&id)
                .and_then(|owner| owner.owner.upgrade())
            {
                assert_eq!(owner.bytes, bytes, "one allocation, one charge");
                owner
            } else {
                let owner = Arc::new(AllocationOwner {
                    id,
                    bytes,
                    hook: LedgerHook::default(),
                    pending: AtomicBool::new(true),
                    registry: Arc::clone(&self.registry),
                });
                state.unqueue(id);
                let pending = state.queue.len();
                state.owners.insert(
                    id,
                    RegistryOwner {
                        owner: Arc::downgrade(&owner),
                        pending: Some(pending),
                    },
                );
                state.owners_capacity = state.owners_capacity.max(state.owners.capacity());
                state.queue.push((id, Arc::downgrade(&owner)));
                self.registry
                    .pending_bytes
                    .fetch_add(bytes, Ordering::AcqRel);
                owner
            }
        };
        self.entries.insert(id, owner);
        self.bytes = self.bytes.checked_add(bytes).expect("allocation byte sum");
        self.metadata = Arc::new(());
    }

    pub(crate) fn remove(&mut self, id: usize) {
        if let Some(owner) = self.entries.remove(&id) {
            self.bytes -= owner.bytes;
            if self.entries.is_empty() {
                self.entries.clear();
            }
            self.metadata = Arc::new(());
        }
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn metadata_allocation(&self) -> (usize, usize) {
        let width = size_of::<usize>() + size_of::<Arc<AllocationOwner>>() + 4 * size_of::<usize>();
        let nodes = if self.entries.is_empty() {
            0
        } else {
            (self.len().saturating_mul(8) + 64).saturating_mul(width)
        };
        (
            Arc::as_ptr(&self.metadata) as usize,
            nodes.saturating_add(arc_bytes::<()>()),
        )
    }

    pub(crate) fn registry_metadata_allocation(&self) -> (usize, usize) {
        self.registry.allocation()
    }

    pub(crate) fn registry_pending_bytes(&self) -> usize {
        self.registry_metadata_allocation()
            .1
            .saturating_sub(self.registry.admitted_bytes.load(Ordering::Acquire))
    }

    pub(crate) fn arm_registry(
        &self,
        queue: &Arc<ReleaseQueue>,
        event: LedgerEvent,
        admitted_bytes: usize,
    ) {
        self.registry.hook.arm(queue, event);
        self.registry
            .admitted_bytes
            .store(admitted_bytes, Ordering::Release);
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.registry
            .state
            .lock()
            .expect("allocation registry")
            .queue
            .len()
    }

    pub(crate) fn reservation(&self, inserts: usize) -> usize {
        let state = self.registry.state.lock().expect("allocation registry");
        let count = self.len().saturating_add(inserts);
        let width = size_of::<usize>() + size_of::<Arc<AllocationOwner>>() + 4 * size_of::<usize>();
        let bound = (count.saturating_mul(8) + 64).saturating_mul(width);
        let nodes = bound.saturating_add(
            inserts
                .saturating_mul(count.max(1).ilog2() as usize + 4)
                .saturating_mul(64 * width)
                .min(bound),
        );
        let registry = (arc_bytes::<Registry>() + MUTEX_STORAGE_BYTES)
            .saturating_add(
                state
                    .queue
                    .capacity()
                    .saturating_add(inserts)
                    .saturating_mul(2 * size_of::<(usize, Weak<AllocationOwner>)>()),
            )
            .saturating_add(
                state
                    .owners_capacity
                    .saturating_add(inserts)
                    .saturating_mul(8 * (size_of::<(usize, RegistryOwner)>() + size_of::<usize>())),
            )
            .saturating_add(
                state
                    .queue
                    .len()
                    .saturating_add(state.owners.len())
                    .saturating_add(inserts.saturating_mul(2))
                    .saturating_mul(arc_bytes::<AllocationOwner>()),
            );
        let pending = state
            .queue
            .len()
            .saturating_add(inserts)
            .saturating_mul(2 * size_of::<Arc<AllocationOwner>>());
        nodes
            .saturating_add(registry)
            .saturating_add(pending)
            .saturating_add(arc_bytes::<()>())
    }

    pub(crate) fn pending_bytes(&self) -> usize {
        self.registry.pending_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn pending_capacity_bytes(&self) -> usize {
        let state = self.registry.state.lock().expect("allocation registry");
        state
            .queue
            .len()
            .saturating_mul(2 * size_of::<Arc<AllocationOwner>>())
    }

    pub(crate) fn take_pending(&self) -> Vec<Arc<AllocationOwner>> {
        let (result, held) = {
            let state = self.registry.state.lock().expect("allocation registry");
            let count = state.queue.len();
            let mut result = Vec::with_capacity(count);
            let mut held = Vec::with_capacity(count);
            for (_, weak) in &state.queue {
                if let Some(owner) = weak.upgrade() {
                    if owner.pending.load(Ordering::Acquire)
                        && self
                            .entries
                            .get(&owner.id)
                            .is_some_and(|member| Arc::ptr_eq(member, &owner))
                    {
                        result.push(Arc::clone(&owner));
                    }
                    held.push(owner);
                }
            }
            (result, held)
        };
        drop(held);
        result
    }

    #[cfg(test)]
    pub(crate) fn audit_members(&self) -> Vec<(usize, usize)> {
        self.entries
            .values()
            .map(|owner| (owner.id, owner.bytes))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_ledger::{arc_mirror, MUTEX_STORAGE_MIRROR};

    #[test]
    fn empty_registry_charge_includes_the_platform_mutex_storage() {
        assert_eq!(
            AllocationSet::EMPTY_ALLOCATION_BYTES,
            arc_mirror::<()>() + arc_mirror::<Registry>() + MUTEX_STORAGE_MIRROR
        );
    }

    #[test]
    fn pending_owners_follow_shared_membership_and_arm_once() {
        let mut set = AllocationSet::default();
        set.insert(7, 64);
        let prior = set.clone();
        set.remove(7);
        assert_eq!(set.bytes(), 0);
        assert_eq!(set.pending_bytes(), 64);
        assert!(set.take_pending().is_empty());
        let owners = prior.take_pending();
        assert_eq!(owners.len(), 1);
        let queue = Arc::new(ReleaseQueue::default());
        assert!(owners[0].arm(&queue, LedgerEvent::Chunk(7)));
        assert!(!owners[0].arm(&queue, LedgerEvent::Chunk(7)));
        assert_eq!(set.pending_bytes(), 0);
        assert_eq!(set.pending_count(), 0);
        assert!(prior.take_pending().is_empty());
    }

    #[test]
    fn arming_the_middle_owner_keeps_pending_work_bounded_to_unarmed_owners() {
        let mut set = AllocationSet::default();
        for id in 0..32 {
            set.insert(id, 64);
        }
        let owners = set.take_pending();
        let queue = Arc::new(ReleaseQueue::default());
        for owner in owners.iter().rev() {
            owner.arm(&queue, LedgerEvent::Chunk(owner.id));
        }
        assert_eq!(set.pending_count(), 0);
        set.insert(32, 64);
        assert_eq!(set.pending_count(), 1);
        let pending = set.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, 32);
        drop(pending);
        assert_eq!(set.take_pending().len(), 1);
    }

    #[test]
    fn registry_pending_bytes_exclude_the_admitted_shared_registry() {
        let mut set = AllocationSet::default();
        let (id, bytes) = set.registry_metadata_allocation();
        assert_eq!(set.registry_pending_bytes(), bytes);
        let queue = Arc::new(ReleaseQueue::default());
        set.arm_registry(&queue, LedgerEvent::Chunk(id), bytes);
        assert_eq!(set.registry_pending_bytes(), 0);
        set.insert(3, 16);
        assert!(set.registry_pending_bytes() > 0);
        set.arm_registry(&queue, LedgerEvent::Chunk(id), bytes);
        assert!(set.registry_pending_bytes() > 0);
        let actual = set.registry_metadata_allocation().1;
        set.arm_registry(&queue, LedgerEvent::Chunk(id), actual);
        assert_eq!(set.registry_pending_bytes(), 0);
    }

    #[test]
    fn registry_witness_observes_metadata_without_retaining_the_registry() {
        let mut set = AllocationSet::default();
        let witness = set.registry_witness();
        assert_eq!(
            witness.allocation(),
            Some(set.registry_metadata_allocation())
        );
        set.insert(3, 16);
        let owners = set.take_pending();
        assert_eq!(
            owners[0].registry_id(),
            set.registry_metadata_allocation().0
        );
        drop(owners);
        drop(set);
        assert!(witness.allocation().is_none());
    }

    #[test]
    fn registry_capacity_bound_survives_owner_removal_tombstones() {
        let mut set = AllocationSet::default();
        for id in 0..64 {
            set.insert(id, 16);
        }
        let capacity = set.registry.state.lock().unwrap().owners_capacity;
        for id in 0..64 {
            set.remove(id);
        }
        assert_eq!(set.registry.state.lock().unwrap().owners_capacity, capacity);
        assert_eq!(set.pending_count(), 0);
        assert_eq!(set.pending_bytes(), 0);
    }

    #[test]
    fn dropping_unpublished_members_releases_pending_bytes_and_registry_identity() {
        let mut set = AllocationSet::default();
        set.insert(9, 128);
        set.remove(9);
        assert_eq!(set.pending_bytes(), 0);
        assert!(set.take_pending().is_empty());
        assert!(set.registry.state.lock().unwrap().owners.is_empty());
        set.insert(9, 256);
        assert_eq!(set.pending_bytes(), 256);
        assert_eq!(set.audit_members(), vec![(9, 256)]);
    }
}

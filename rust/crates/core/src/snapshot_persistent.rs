use std::borrow::Borrow;
use std::mem::size_of;
use std::ops::{Index, Range};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::snapshot_ledger::arc_slice_bytes;

#[derive(Debug)]
pub(crate) struct Counted<T> {
    value: T,
    copies: Arc<AtomicUsize>,
}

impl<T: Clone> Clone for Counted<T> {
    fn clone(&self) -> Self {
        self.copies.fetch_add(1, Ordering::Relaxed);
        Self {
            value: self.value.clone(),
            copies: Arc::clone(&self.copies),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Tree<K: Ord, V> {
    entries: imbl::OrdMap<K, Counted<V>>,
    copies: Arc<AtomicUsize>,
    metadata: Arc<()>,
    key_bytes: usize,
    max_key_bytes: usize,
}

impl<K: Ord + Clone + Borrow<str>, V: Clone> Default for Tree<K, V> {
    fn default() -> Self {
        Self {
            entries: imbl::OrdMap::new(),
            copies: Arc::new(AtomicUsize::new(0)),
            metadata: Arc::new(()),
            key_bytes: 0,
            max_key_bytes: 0,
        }
    }
}

impl<K: Ord + Clone + Borrow<str>, V: Clone> Tree<K, V> {
    fn metadata_bound(len: usize, max_key_bytes: usize) -> usize {
        if len == 0 {
            return 5 * size_of::<usize>();
        }
        let width = size_of::<K>() + size_of::<Counted<V>>() + 4 * size_of::<usize>();
        (len.saturating_mul(8) + 64)
            .saturating_mul(width)
            .saturating_add(len.saturating_mul(max_key_bytes))
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        let key = if let Some((existing, _)) = self.entries.get_key_value(&key) {
            existing.clone()
        } else {
            let text: &str = key.borrow();
            let bytes = arc_slice_bytes::<u8>(text.len());
            self.key_bytes += bytes;
            self.max_key_bytes = self.max_key_bytes.max(bytes);
            key
        };
        self.metadata = Arc::new(());
        self.entries.insert(
            key,
            Counted {
                value,
                copies: Arc::clone(&self.copies),
            },
        );
    }

    pub(crate) fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.entries.get(key).map(|entry| &entry.value)
    }

    pub(crate) fn contains_key<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.entries.contains_key(key)
    }

    pub(crate) fn remove_entry<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
    {
        let owned = self.entries.get_key_value(key)?.0.clone();
        let value = self.entries.remove(key)?.value;
        if self.entries.is_empty() {
            self.entries.clear();
            self.max_key_bytes = 0;
        }
        let text: &str = owned.borrow();
        self.key_bytes -= arc_slice_bytes::<u8>(text.len());
        self.metadata = Arc::new(());
        Some((owned, value))
    }

    pub(crate) fn key_allocation<Q: Ord + ?Sized>(
        &self,
        key: &Q,
        allocation: fn(&K) -> Option<(usize, usize)>,
    ) -> Option<(usize, usize)>
    where
        K: Borrow<Q>,
    {
        self.entries
            .get_key_value(key)
            .and_then(|(key, _)| allocation(key))
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn copied(&self) -> usize {
        self.copies.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn metadata_id(&self) -> usize {
        Arc::as_ptr(&self.metadata) as usize
    }
    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    pub(crate) fn allocations(
        &self,
        key_allocation: fn(&K) -> Option<(usize, usize)>,
    ) -> impl Iterator<Item = (usize, usize)> + '_ {
        std::iter::once((
            Arc::as_ptr(&self.metadata) as usize,
            Self::metadata_bound(self.len(), self.max_key_bytes),
        ))
        .chain(self.entries.keys().filter_map(key_allocation))
    }

    pub(crate) fn bytes_since(
        &self,
        previous: Option<&Self>,
        _key_allocation: fn(&K) -> Option<(usize, usize)>,
    ) -> usize {
        if previous.is_some_and(|previous| self.entries.ptr_eq(&previous.entries)) {
            0
        } else {
            Self::metadata_bound(self.len(), self.max_key_bytes)
                + self
                    .key_bytes
                    .saturating_sub(previous.map_or(0, |previous| previous.key_bytes))
        }
    }

    pub(crate) fn key_reservation(&self, inserts: usize, incoming_max: usize) -> usize {
        self.len()
            .saturating_add(inserts)
            .saturating_mul(incoming_max.saturating_sub(self.max_key_bytes))
    }

    #[cfg(test)]
    pub(crate) fn key_payload_max(&self) -> usize {
        self.max_key_bytes
    }

    pub(crate) fn reservation(&self, inserts: usize) -> usize {
        if inserts == 0 {
            return 0;
        }
        let count = self.len().saturating_add(inserts);
        let bound = Self::metadata_bound(count, self.max_key_bytes);
        bound.saturating_add(
            inserts
                .saturating_mul(count.max(1).ilog2() as usize + 4)
                .saturating_mul(
                    64 * (size_of::<K>() + size_of::<Counted<V>>() + 4 * size_of::<usize>()),
                )
                .min(bound),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Vector<T> {
    entries: imbl::Vector<Counted<T>>,
    copies: Arc<AtomicUsize>,
    metadata: Arc<()>,
}

impl<T: Clone> Default for Vector<T> {
    fn default() -> Self {
        Self {
            entries: imbl::Vector::new(),
            copies: Arc::new(AtomicUsize::new(0)),
            metadata: Arc::new(()),
        }
    }
}

impl<T: Clone> Vector<T> {
    fn metadata_bound(len: usize) -> usize {
        if len == 0 {
            return 5 * size_of::<usize>();
        }
        (len.saturating_mul(8) + 256)
            .saturating_mul(size_of::<Counted<T>>() + 4 * size_of::<usize>())
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        self.entries.get(at).map(|entry| &entry.value)
    }
    pub(crate) fn last(&self) -> Option<&T> {
        self.len().checked_sub(1).and_then(|at| self.get(at))
    }
    pub(crate) fn last_mut(&mut self) -> Option<&mut T> {
        self.metadata = Arc::new(());
        self.entries.back_mut().map(|entry| &mut entry.value)
    }
    pub(crate) fn push(&mut self, value: T) {
        self.metadata = Arc::new(());
        self.entries.push_back(Counted {
            value,
            copies: Arc::clone(&self.copies),
        });
    }
    pub(crate) fn extend(&mut self, values: impl IntoIterator<Item = T>) {
        for value in values {
            self.push(value);
        }
    }
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        self.entries.iter().map(|entry| &entry.value)
    }
    pub(crate) fn copied(&self) -> usize {
        self.copies.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn metadata_id(&self) -> usize {
        Arc::as_ptr(&self.metadata) as usize
    }
    pub(crate) fn allocations(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        std::iter::once((
            Arc::as_ptr(&self.metadata) as usize,
            Self::metadata_bound(self.len()),
        ))
    }
    pub(crate) fn bytes_since(&self, previous: Option<&Self>) -> usize {
        if previous.is_some_and(|previous| self.entries.ptr_eq(&previous.entries)) {
            0
        } else {
            Self::metadata_bound(self.len())
        }
    }
    pub(crate) fn reservation(&self, inserts: usize) -> usize {
        if inserts == 0 {
            return 0;
        }
        let count = self.len().saturating_add(inserts);
        let bound = Self::metadata_bound(count);
        bound.saturating_add(
            inserts
                .saturating_mul(count.max(1).ilog2() as usize + 4)
                .saturating_mul(256 * (size_of::<Counted<T>>() + 4 * size_of::<usize>()))
                .min(bound),
        )
    }
    pub(crate) fn partition_point(&self, mut predicate: impl FnMut(&T) -> bool) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let at = low + (high - low) / 2;
            if predicate(&self[at]) {
                low = at + 1;
            } else {
                high = at;
            }
        }
        low
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.metadata = Arc::new(());
            if len == 0 {
                self.entries.clear();
            } else {
                self.entries.truncate(len);
            }
        }
    }
    pub(crate) fn binary_search_by_key<B: Ord>(
        &self,
        key: &B,
        mut extract: impl FnMut(&T) -> B,
    ) -> Result<usize, usize> {
        self.entries
            .binary_search_by_key(key, |entry| extract(&entry.value))
    }
    pub(crate) fn range(
        &self,
        range: Range<usize>,
    ) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
        range.map(move |at| &self[at])
    }
}

impl<T: Clone> From<Vec<T>> for Vector<T> {
    fn from(values: Vec<T>) -> Self {
        let mut result = Self::default();
        result.extend(values);
        result
    }
}

impl<T: Clone> IntoIterator for Vector<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries
            .into_iter()
            .map(|entry| entry.value)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

impl<T: Clone> Index<usize> for Vector<T> {
    type Output = T;
    fn index(&self, at: usize) -> &T {
        self.get(at).expect("persistent vector index")
    }
}

pub(crate) fn string_allocation(key: &Arc<str>) -> Option<(usize, usize)> {
    Some((
        Arc::as_ptr(key) as *const u8 as usize,
        arc_slice_bytes::<u8>(key.len()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_ledger::arc_slice_mirror;

    #[test]
    fn removed_separator_payload_is_covered_without_a_retired_key_pool() {
        let mut tree = Tree::<Arc<str>, usize>::default();
        let mut retired = None;
        for index in 0..17 {
            let key = Arc::<str>::from(format!(
                "{index:02}-{}",
                "x".repeat(if index == 8 { 4096 } else { 1 })
            ));
            if index == 8 {
                retired = Some(Arc::downgrade(&key));
            }
            tree.insert(key, index);
        }
        let key = format!("08-{}", "x".repeat(4096));
        tree.remove_entry(key.as_str()).unwrap();
        let retired = retired
            .unwrap()
            .upgrade()
            .expect("B+tree separator retains the key");
        assert!(!tree.contains_key(retired.as_ref()));
        let expected_max = arc_slice_mirror::<u8>(4099);
        assert_eq!(tree.max_key_bytes, expected_max);
        let metadata = tree.allocations(string_allocation).next().unwrap().1;
        let width = size_of::<Arc<str>>() + size_of::<Counted<usize>>() + 4 * size_of::<usize>();
        let expected_metadata = (tree.len() * 8 + 64) * width + tree.len() * expected_max;
        assert_eq!(metadata, expected_metadata);
        let incoming_max = arc_slice_mirror::<u8>(8195);
        assert_eq!(
            tree.key_reservation(1, incoming_max),
            (tree.len() + 1) * (incoming_max - expected_max)
        );
        assert!(tree.clone().key_payload_max() >= arc_slice_mirror::<u8>(retired.len()));
    }

    #[test]
    fn truncating_persistent_vectors_to_zero_releases_heap_chunks() {
        for count in [1, 64, 1024] {
            let mut vector = Vector::<usize>::default();
            vector.extend(0..count);
            let prior = vector.clone();
            vector.truncate(0);
            assert!(vector.entries.is_inline());
            assert_eq!(vector.len(), 0);
            assert_eq!(prior.len(), count);
            assert_eq!(prior[0], 0);
            assert_eq!(vector.bytes_since(None), 5 * size_of::<usize>());
        }
    }

    #[test]
    fn persistent_string_keys_charge_the_padded_arc_layout() {
        for len in [0, 1, 7, 8, 9, 31] {
            let key = Arc::<str>::from("x".repeat(len));
            let expected = arc_slice_mirror::<u8>(len);
            assert_eq!(string_allocation(&key).unwrap().1, expected);
            let mut tree = Tree::<Arc<str>, usize>::default();
            tree.insert(Arc::clone(&key), 1);
            assert_eq!(tree.key_bytes, expected);
            tree.remove_entry(key.as_ref()).unwrap();
            assert_eq!(tree.key_bytes, 0);
            assert!(tree.entries.ptr_eq(&imbl::OrdMap::new()));
        }
    }
}

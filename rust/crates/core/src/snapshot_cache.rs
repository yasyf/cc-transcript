impl StoreState {
    fn next_cache_touch(&mut self) -> u64 {
        self.cache_clock = self.cache_clock.checked_add(1).expect("cache sequence");
        self.cache_clock
    }

    fn touch_generation(&mut self, key: usize) {
        let touched = self.next_cache_touch();
        let mut record = self.generations.get_mut(&key).expect("cached generation");
        let cached = record.cached.as_mut().expect("cached owner");
        assert!(self.cache_order.remove(&(cached.touched, CacheOwner::Generation(key))));
        cached.touched = touched;
        assert!(self.cache_order.insert((touched, CacheOwner::Generation(key))));
    }

    fn cached_snapshot(&mut self, key: usize) -> Arc<TranscriptSnapshot> {
        self.touch_generation(key);
        Arc::clone(&self.generations.get(&key).expect("cached generation").cached.as_ref().expect("cached owner").snapshot)
    }

    fn latest_source(&mut self, identity: &SourceIdentity) -> Option<Arc<TranscriptSnapshot>> {
        let key = *self.latest.get(identity)?;
        Some(self.cached_snapshot(key))
    }

    fn source_spine(&mut self, identity: &SourceIdentity) -> Option<Arc<TranscriptSnapshot>> {
        let key = *self.source_spines.get(&identity.file())?;
        Some(self.cached_snapshot(key))
    }

    fn forget_source_spine(&mut self, identity: &SourceIdentity, snapshot: &Arc<TranscriptSnapshot>) {
        let identity = identity.file();
        let key = snapshot_key(snapshot);
        if self.source_spines.get(&identity) == Some(&key) {
            self.source_spines.remove(&identity);
            self.generations.get_mut(&key).expect("source spine generation").cached.as_mut().expect("cached owner").spine = None;
            self.release_unused_cached_generation(key);
        }
    }

    fn cache_owner_growth(&self, snapshot: &Arc<TranscriptSnapshot>) -> usize {
        if self.generations.get(&snapshot_key(snapshot)).is_some_and(|record| record.cached.is_some()) {
            0
        } else {
            size_of::<(u64, CacheOwner)>()
        }
    }

    fn latest_growth(&self, identity: &SourceIdentity, snapshot: &Arc<TranscriptSnapshot>) -> usize {
        self.latest.growth_for(identity)
            + self.source_spines.growth_for(&identity.file())
            + self.cache_owner_growth(snapshot)
    }

    fn classified_growth(&self, key: &String, snapshot: &Arc<TranscriptSnapshot>) -> usize {
        self.classified.growth_for(key) + key.capacity() + key.len() + self.cache_owner_growth(snapshot)
    }

    fn cache_generation(&mut self, snapshot: Arc<TranscriptSnapshot>) -> usize {
        let key = snapshot_key(&snapshot);
        if self.generations.get(&key).expect("registered generation").cached.is_none() {
            let touched = self.next_cache_touch();
            self.generations.get_mut(&key).expect("registered generation").cached = Some(CachedSnapshot {
                snapshot,
                latest: None,
                spine: None,
                classified: None,
                touched,
            });
            assert!(self.cache_order.insert((touched, CacheOwner::Generation(key))));
        } else {
            self.touch_generation(key);
        }
        key
    }

    fn release_unused_cached_generation(&mut self, key: usize) {
        let mut record = self.generations.get_mut(&key).expect("cached generation");
        if record.cached.as_ref().is_some_and(|cached| cached.latest.is_none() && cached.spine.is_none() && cached.classified.is_none()) {
            let cached = record.cached.take().expect("cached owner");
            assert!(self.cache_order.remove(&(cached.touched, CacheOwner::Generation(key))));
            drop(cached);
        }
    }

    fn insert_classified(&mut self, key: String, snapshot: Arc<TranscriptSnapshot>) {
        let generation = self.cache_generation(snapshot);
        self.classified.reserve_for(&key);
        if let Some(previous) = self.classified.insert(key.clone(), generation).filter(|previous| *previous != generation) {
            self.generations.get_mut(&previous).expect("classified generation").cached.as_mut().expect("cached owner").classified = None;
            self.release_unused_cached_generation(previous);
        }
        self.generations.get_mut(&generation).expect("classified generation").cached.as_mut().expect("cached owner").classified = Some(key);
    }

    fn classified_source(&mut self, key: &str) -> Option<Arc<TranscriptSnapshot>> {
        let generation = *self.classified.get(key)?;
        Some(self.cached_snapshot(generation))
    }

    #[cfg(test)]
    fn clear_classified(&mut self) {
        loop {
            let Some(key) = self.classified.keys().next().cloned() else { break; };
            let generation = self.classified.remove(&key).expect("classified alias");
            self.generations.get_mut(&generation).expect("classified generation").cached.as_mut().expect("cached owner").classified = None;
            self.release_unused_cached_generation(generation);
        }
    }

    fn next_cache_victim(&self) -> Option<CacheOwner> {
        self.cache_order.iter().find_map(|(_, owner)| {
            self.ledger.shared.work().tick(1);
            let available = match owner {
                CacheOwner::Generation(key) => Arc::strong_count(&self.generations.get(key).expect("cached generation").cached.as_ref().expect("cached owner").snapshot) == 1,
                CacheOwner::Carried(key) => {
                    let metadata = self.carried_owners.get(key).expect("carried owner");
                    let carried = self.carried_classifications.get(&metadata.lineages[0]).expect("carried alias");
                    Arc::strong_count(carried) == metadata.lineages.len()
                }
            };
            available.then_some(*owner)
        })
    }

    fn evict_cache_owner(&mut self, owner: CacheOwner) {
        match owner {
            CacheOwner::Generation(key) => {
                let cached = self.generations.get_mut(&key).expect("cached generation").cached.take().expect("cached owner");
                assert!(self.cache_order.remove(&(cached.touched, owner)));
                if let Some(identity) = cached.latest {
                    assert_eq!(self.latest.remove(&identity), Some(key));
                    if self.recent_codex.contains_key(&identity) {
                        self.recent_codex_raw_bytes -= codex_raw_len(&cached.snapshot);
                    }
                }
                if let Some(identity) = cached.spine {
                    assert_eq!(self.source_spines.remove(&identity), Some(key));
                }
                if let Some(classified) = cached.classified {
                    assert_eq!(self.classified.remove(&classified), Some(key));
                }
                drop(cached.snapshot);
            }
            CacheOwner::Carried(key) => {
                let metadata = self.carried_owners.remove(&key).expect("carried owner");
                assert!(self.cache_order.remove(&(metadata.touched, owner)));
                for lineage in metadata.lineages {
                    let carried = self.carried_classifications.remove(&lineage).expect("carried alias");
                    for anchor in carried.anchors() {
                        self.ledger.shared.release(anchor.id);
                    }
                }
            }
        }
        self.drain();
    }
    fn carried_owner_growth(&self, lineage: &(SourceIdentity, String), carried: &Arc<CarriedClassification>) -> usize {
        let key = Arc::as_ptr(carried) as usize;
        if let Some(owner) = self.carried_owners.get(&key) {
            if owner.lineages.contains(lineage) { 0 } else { vec_growth(&owner.lineages, 1) + lineage.1.len() }
        } else {
            self.carried_owners.growth_for(&key)
                + size_of::<(u64, CacheOwner)>()
                + size_of::<(SourceIdentity, String)>()
                + lineage.1.len()
        }
    }

    fn attach_carried_owner(&mut self, lineage: &(SourceIdentity, String), carried: &Arc<CarriedClassification>) {
        let key = Arc::as_ptr(carried) as usize;
        let touched = self.next_cache_touch();
        if let Some(mut owner) = self.carried_owners.get_mut(&key) {
            assert!(self.cache_order.remove(&(owner.touched, CacheOwner::Carried(key))));
            owner.lineages.push(lineage.clone());
            owner.touched = touched;
        } else {
            self.carried_owners.reserve_for(&key);
            self.carried_owners.insert(key, CarriedOwner { lineages: vec![lineage.clone()], touched });
        }
        assert!(self.cache_order.insert((touched, CacheOwner::Carried(key))));
    }

    fn detach_carried_owner(&mut self, lineage: &(SourceIdentity, String), carried: &Arc<CarriedClassification>) {
        let key = Arc::as_ptr(carried) as usize;
        let mut owner = self.carried_owners.get_mut(&key).expect("carried owner");
        let at = owner.lineages.iter().position(|alias| alias == lineage).expect("carried owner alias");
        owner.lineages.swap_remove(at);
        let empty = owner.lineages.is_empty();
        let touched = owner.touched;
        drop(owner);
        if empty {
            self.carried_owners.remove(&key);
            assert!(self.cache_order.remove(&(touched, CacheOwner::Carried(key))));
        }
    }

    fn touch_carried_owner(&mut self, carried: &Arc<CarriedClassification>) {
        let key = Arc::as_ptr(carried) as usize;
        let touched = self.next_cache_touch();
        let mut owner = self.carried_owners.get_mut(&key).expect("carried owner");
        assert!(self.cache_order.remove(&(owner.touched, CacheOwner::Carried(key))));
        owner.touched = touched;
        assert!(self.cache_order.insert((touched, CacheOwner::Carried(key))));
    }

    #[cfg(test)]
    fn assert_cache_owners(&self) {
        let cached = self.generations.values().filter(|record| record.cached.is_some()).count();
        assert_eq!(self.cache_order.len(), cached + self.carried_owners.len());
        for (key, record) in self.generations.iter() {
            if let Some(cached) = &record.cached {
                assert_eq!(*key, snapshot_key(&cached.snapshot));
                assert!(self.cache_order.contains(&(cached.touched, CacheOwner::Generation(*key))));
                if let Some(identity) = cached.latest { assert_eq!(self.latest.get(&identity), Some(key)); }
                if let Some(identity) = cached.spine { assert_eq!(self.source_spines.get(&identity), Some(key)); }
                if let Some(classified) = &cached.classified { assert_eq!(self.classified.get(classified), Some(key)); }
            }
        }
        for (identity, key) in self.latest.iter() {
            assert_eq!(self.generations.get(key).unwrap().cached.as_ref().unwrap().latest, Some(*identity));
        }
        for (identity, key) in self.source_spines.iter() {
            assert_eq!(self.generations.get(key).unwrap().cached.as_ref().unwrap().spine, Some(*identity));
        }
        for (classified, key) in self.classified.iter() {
            assert_eq!(self.generations.get(key).unwrap().cached.as_ref().unwrap().classified.as_ref(), Some(classified));
        }
        let mut aliases = 0;
        for (key, owner) in self.carried_owners.iter() {
            assert!(self.cache_order.contains(&(owner.touched, CacheOwner::Carried(*key))));
            assert!(!owner.lineages.is_empty());
            for lineage in &owner.lineages {
                assert_eq!(Arc::as_ptr(self.carried_classifications.get(lineage).unwrap()) as usize, *key);
                aliases += 1;
            }
        }
        assert_eq!(aliases, self.carried_classifications.len());
    }

}

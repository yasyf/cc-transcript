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
        let key = self.source_spines.get(&identity.file())?.first()?.generation;
        Some(self.cached_snapshot(key))
    }

    fn remove_source_view(&mut self, snapshot: &Arc<TranscriptSnapshot>) {
        let identity = snapshot.stamp.identity.file();
        let views = self.source_spines.get_mut(&identity).expect("source views");
        assert!(views.remove(&SourceView::of(snapshot)));
        self.source_view_nodes -= 1;
        if views.is_empty() { self.source_spines.remove(&identity); }
    }

    fn forget_source_spine(&mut self, identity: &SourceIdentity, snapshot: &Arc<TranscriptSnapshot>) {
        let identity = identity.file();
        let key = snapshot_key(snapshot);
        let selected = self.generations.get(&key).and_then(|record| record.cached.as_ref()).is_some_and(|cached| cached.spine == Some(identity));
        if selected {
            self.remove_source_view(snapshot);
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
        let indexed = self.generations.get(&snapshot_key(snapshot)).and_then(|record| record.cached.as_ref()).is_some_and(|cached| cached.spine.is_some());
        self.latest.growth_for(identity)
            + self.source_spines.growth_for(&identity.file())
            + if indexed { 0 } else { size_of::<SourceView>() }
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
                CacheOwner::Catalog(key) => {
                    let metadata = self.catalog_owners.get(key).expect("catalog owner");
                    Arc::strong_count(self.catalogs.get(metadata.key.as_str()).expect("catalog slot")) == 1
                }
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
            CacheOwner::Catalog(key) => {
                let metadata = self.detach_catalog_owner(key);
                let slot = self.catalogs.remove(metadata.key.as_str()).expect("catalog slot");
                drop(slot);
            }
            CacheOwner::Generation(key) => {
                let cached = self.generations.get_mut(&key).expect("cached generation").cached.take().expect("cached owner");
                assert!(self.cache_order.remove(&(cached.touched, owner)));
                if let Some(identity) = cached.latest {
                    assert_eq!(self.latest.remove(&identity), Some(key));
                    if self.recent_codex.contains_key(&identity) {
                        self.recent_codex_raw_bytes -= codex_raw_len(&cached.snapshot);
                    }
                }
                if cached.spine.is_some() {
                    self.remove_source_view(&cached.snapshot);
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
        assert_eq!(self.cache_order.len(), cached + self.carried_owners.len() + self.catalog_owners.len());
        assert_eq!(self.catalog_owners.len(), self.catalogs.len());
        for (key, owner) in self.catalog_owners.iter() {
            assert_eq!(Arc::as_ptr(self.catalogs.get(owner.key.as_str()).unwrap()) as usize, *key);
            assert!(self.cache_order.contains(&(owner.touched, CacheOwner::Catalog(*key))));
        }
        for (key, record) in self.generations.iter() {
            if let Some(cached) = &record.cached {
                assert_eq!(*key, snapshot_key(&cached.snapshot));
                assert!(self.cache_order.contains(&(cached.touched, CacheOwner::Generation(*key))));
                if let Some(identity) = cached.latest { assert_eq!(self.latest.get(&identity), Some(key)); }
                if let Some(identity) = cached.spine { assert!(self.source_spines.get(&identity).unwrap().contains(&SourceView::of(&cached.snapshot))); }
                if let Some(classified) = &cached.classified { assert_eq!(self.classified.get(classified), Some(key)); }
            }
        }
        for (identity, key) in self.latest.iter() {
            assert_eq!(self.generations.get(key).unwrap().cached.as_ref().unwrap().latest, Some(*identity));
        }
        let mut source_nodes = 0;
        for (identity, views) in self.source_spines.iter() {
            assert!(!views.is_empty());
            for view in views {
                let cached = self.generations.get(&view.generation).unwrap().cached.as_ref().unwrap();
                assert_eq!(cached.spine, Some(*identity));
                assert!(SourceView::of(&cached.snapshot) == *view);
                source_nodes += 1;
            }
        }
        assert_eq!(self.source_view_nodes, source_nodes);
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

    fn catalog_owner_growth(&self, key: &String, slot: &Arc<CatalogSlot>) -> usize {
        let identity = Arc::as_ptr(slot) as usize;
        if self.catalog_owners.contains_key(&identity) { 0 } else {
            self.catalog_owners.growth_for(&identity) + key.len() + size_of::<(u64, CacheOwner)>()
        }
    }

    fn attach_catalog_owner(&mut self, key: &String, slot: &Arc<CatalogSlot>) {
        let identity = Arc::as_ptr(slot) as usize;
        let touched = self.next_cache_touch();
        self.catalog_owners.reserve_for(&identity);
        assert!(self.catalog_owners.insert(identity, CatalogOwner { key: key.clone(), touched }).is_none());
        assert!(self.cache_order.insert((touched, CacheOwner::Catalog(identity))));
    }

    fn touch_catalog_owner(&mut self, slot: &Arc<CatalogSlot>) {
        let identity = Arc::as_ptr(slot) as usize;
        let touched = self.next_cache_touch();
        let mut owner = self.catalog_owners.get_mut(&identity).expect("catalog owner");
        assert!(self.cache_order.remove(&(owner.touched, CacheOwner::Catalog(identity))));
        owner.touched = touched;
        assert!(self.cache_order.insert((touched, CacheOwner::Catalog(identity))));
    }

    fn detach_catalog_owner(&mut self, identity: usize) -> CatalogOwner {
        let owner = self.catalog_owners.remove(&identity).expect("catalog owner");
        assert!(self.cache_order.remove(&(owner.touched, CacheOwner::Catalog(identity))));
        owner
    }

    fn remove_catalog(&mut self, key: &str) -> Option<Arc<CatalogSlot>> {
        let slot = self.catalogs.remove(key)?;
        let identity = Arc::as_ptr(&slot) as usize;
        let owner = self.detach_catalog_owner(identity);
        assert_eq!(owner.key, key);
        Some(slot)
    }

}

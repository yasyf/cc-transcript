impl NativeStore {
    #[cfg(test)]
    fn audit_prepared_graph_bytes(graph_id: &String, graph: &Arc<Mutex<PreparedGraph>>) -> usize {
        let graph = graph.lock().expect("prepared graph");
        graph_id.capacity()
            + arc_mirror::<Mutex<PreparedGraph>>()
            + MUTEX_STORAGE_MIRROR
            + graph.claimant.capacity()
            + graph.registry_generation.capacity()
            + graph.admission.capacity()
            + graph.revision.capacity()
            + value_bytes(&graph.authority)
            + value_bytes(&graph.root_handle)
            + value_bytes(&graph.classifier)
            + graph.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
            + graph
                .stamps
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + graph.root_slices.reserved_bytes()
            + graph
                .root_slices
                .keys()
                .map(String::capacity)
                .sum::<usize>()
    }

    #[cfg(test)]
    fn audit_prepared_build_bytes(token: &String, build: &PreparedBuild) -> usize {
        token.capacity()
            + size_of::<PreparedBuild>()
            + build.claimant.capacity()
            + value_bytes(&build.context)
            + value_bytes(&build.request)
            + value_bytes(&build.root_handle)
            + value_bytes(&build.classifier)
            + build.tasks.capacity() * size_of::<GraphTask>()
            + build
                .tasks
                .iter()
                .map(|task| match task {
                    GraphTask::Visit {
                        path,
                        depth: _,
                        spawned_by,
                    } => path.capacity() + spawned_by.as_ref().map_or(0, String::capacity),
                    GraphTask::List { parent, depth: _ } => parent.capacity(),
                })
                .sum::<usize>()
            + build.listing.as_ref().map_or(
                0,
                |GraphListing {
                     entries,
                     children,
                     depth: _,
                 }| {
                    children.capacity() * size_of::<PathBuf>()
                        + children.iter().map(PathBuf::capacity).sum::<usize>()
                        + Self::audit_open_directory_bytes(entries)
                },
            )
            + build.seen.capacity() * size_of::<SourceIdentity>()
            + build.sources.capacity() * size_of::<PreparedSourceRef>()
            + build
                .sources
                .iter()
                .map(|PreparedSourceRef { path, stamp: _ }| path.capacity())
                .sum::<usize>()
            + build.stamps.capacity() * size_of::<(PathBuf, SourceStamp)>()
            + build
                .stamps
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + build.sidechain_dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + build
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
    }

    #[cfg(test)]
    pub(crate) fn audit_value_bytes(value: &Value) -> usize {
        match value.get_type() {
            JsonType::Null | JsonType::Boolean => 0,
            JsonType::Number => value
                .as_raw_number()
                .map_or(0, |number| number.as_str().len()),
            JsonType::String => value.as_str().unwrap().len(),
            JsonType::Array => {
                let array = value.as_array().unwrap();
                array.capacity() * size_of::<Value>()
                    + array.iter().map(Self::audit_value_bytes).sum::<usize>()
            }
            JsonType::Object => {
                let object = value.as_object().unwrap();
                object.capacity() * size_of::<(Value, Value)>()
                    + object
                        .iter()
                        .map(|(key, item)| key.len() + Self::audit_value_bytes(item))
                        .sum::<usize>()
            }
        }
    }

    #[cfg(test)]
    fn audit_facts_bytes(facts: &crate::snapshot_prepared::PreparedFacts) -> usize {
        let crate::snapshot_prepared::PreparedFacts {
            inputs,
            has_error: _,
            overrides,
            accounted: _,
        } = facts;
        let override_bytes = match overrides {
            crate::snapshot_prepared::OverrideFacts::Loaded(events) => {
                events.capacity() * size_of::<crate::snapshot_prepared::OverrideEvent>()
                    + events
                        .iter()
                        .map(|crate::snapshot_prepared::OverrideEvent { text, tools }| {
                            text.capacity()
                                + tools.capacity() * size_of::<String>()
                                + tools.iter().map(String::capacity).sum::<usize>()
                        })
                        .sum::<usize>()
            }
            crate::snapshot_prepared::OverrideFacts::Unloaded
            | crate::snapshot_prepared::OverrideFacts::Exceeded => 0,
        };
        arc_mirror::<crate::snapshot_prepared::PreparedFacts>()
            + Self::audit_value_bytes(inputs)
            + override_bytes
    }

    #[cfg(test)]
    fn audit_prepared_fact_bytes(state: &StoreState) -> usize {
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let mut add = |facts: &Arc<crate::snapshot_prepared::PreparedFacts>| {
            if seen.insert(Arc::as_ptr(facts) as usize) {
                bytes += Self::audit_facts_bytes(facts);
            }
        };
        for cached in state.prepared_facts.values() {
            add(&cached.facts);
        }
        for graph in state.prepared_graphs.values() {
            let graph = graph.lock().expect("prepared graph");
            add(&graph.root_facts);
            for facts in graph.root_slices.values() {
                add(facts);
            }
        }
        for build in state.prepared_builds.values() {
            add(&build.root_facts);
        }
        for owner in &state.retained_owners {
            add(&owner.upgrade().expect("retained facts are live"));
        }
        bytes
    }

    #[cfg(test)]
    fn audit_warm_buffer_bytes(state: &StoreState) -> usize {
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let mut add = |key: usize, buffer_bytes: usize| {
            if seen.insert(key) {
                bytes += buffer_bytes;
            }
        };
        let sources = |sources: &Arc<[PreparedSourceRef]>| {
            arc_slice_mirror::<PreparedSourceRef>(sources.len())
                + sources
                    .iter()
                    .map(|PreparedSourceRef { path, stamp: _ }| path.capacity())
                    .sum::<usize>()
        };
        let dirs = |dirs: &Arc<[(PathBuf, Option<SourceStamp>)]>| {
            arc_slice_mirror::<(PathBuf, Option<SourceStamp>)>(dirs.len())
                + dirs.iter().map(|(path, _)| path.capacity()).sum::<usize>()
        };
        for membership in state.warm_memberships.values() {
            add(slice_key(&membership.members), sources(&membership.members));
            add(
                slice_key(&membership.positions),
                arc_slice_mirror::<(SourceIdentity, usize)>(membership.positions.len()),
            );
            add(
                slice_key(&membership.sidechain_dirs),
                dirs(&membership.sidechain_dirs),
            );
        }
        for graph in state.prepared_graphs.values() {
            let graph = graph.lock().expect("prepared graph");
            add(slice_key(&graph.sources), sources(&graph.sources));
            add(
                slice_key(&graph.sidechain_dirs),
                dirs(&graph.sidechain_dirs),
            );
        }
        bytes
    }

    fn prepared_fact_budget(&self) -> usize {
        self.config.retained.min(self.config.prepared_fact_memory)
    }

    fn cache_prepared_facts(
        &self,
        stamp: SourceStamp,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        classifier: &Value,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        need: FactsNeed,
    ) -> Result<RetainedFacts<'_>, SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let authority = &context["authority"];
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            registry_generation,
            admission,
            authority,
            classifier,
        )?;
        let mut state = self.lock_state();
        if let Some(cached) = state.touch_prepared_facts(
            &key,
            stamp,
            registry_generation,
            admission,
            authority,
            classifier,
            need,
        ) {
            return Ok(self.retain_cached_facts(&mut state, cached));
        }
        state.retire_identity_facts(stamp.identity, |cached| cached.stamp == stamp);
        let accounted = facts.accounted_bytes();
        let budget = self.prepared_fact_budget();
        if accounted > budget {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "prepared facts cache budget exhausted",
            ));
        }
        let mut used = state.ledger.shared.facts();
        while accounted > budget.saturating_sub(used) {
            let Some(oldest) = state.evictable_prepared_facts() else {
                return Err(SnapshotError::new(
                    Status::RetainedLimit,
                    "prepared facts cache budget exhausted",
                ));
            };
            state.remove_prepared_facts(&oldest).expect("cached fact");
            used = state.ledger.shared.facts();
        }
        let cached = CachedPreparedFacts {
            stamp,
            registry_generation: registry_generation.to_owned(),
            admission: admission.to_owned(),
            authority: authority.clone(),
            classifier: classifier.clone(),
            facts: Arc::clone(&facts),
            last_used: now_ms(),
        };
        let additional = state.admission(&key, &cached, [facts_anchor(&cached.facts)])
            + state.prepared_facts_growth(&key);
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_prepared_facts(key, cached);
        Ok(self.retain_cached_facts(&mut state, facts))
    }

    fn admit_prepared_disk_growth(&self, context: &Value, growth: usize) -> bool {
        let mut state = self.lock_state();
        if self.admit_memory(&mut state, context, growth).is_err() {
            return false;
        }
        state.prepared_disk_index_bytes += growth;
        true
    }

    fn prepared_root_facts(
        &self,
        root: &Arc<TranscriptSnapshot>,
        classifier: &Value,
        context: &Value,
        remaining: &WorkLimits,
        cache_reads: PreparedCacheReads<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        need: FactsNeed,
    ) -> Result<(RetainedFacts<'_>, ProjectionReservation<'_>), SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            root.stamp,
            registry_generation,
            admission,
            &context["authority"],
            classifier,
        )?;
        if let Some(retained) = self.touch_retained_facts(
            &key,
            root.stamp,
            registry_generation,
            admission,
            &context["authority"],
            classifier,
            need,
        ) {
            self.after_facts_returned();
            return Ok((
                retained,
                ProjectionReservation {
                    store: self,
                    bytes: 0,
                },
            ));
        }
        let bound = crate::snapshot_projection::facts_bound(root, need);
        loop {
            let claim = self.claim_fact_read(cancel, remaining.deadline_unix_ms)?;
            if let Some(retained) = self.touch_retained_facts(
                &key,
                root.stamp,
                registry_generation,
                admission,
                &context["authority"],
                classifier,
                need,
            ) {
                self.after_facts_returned();
                return Ok((
                    retained,
                    ProjectionReservation {
                        store: self,
                        bytes: 0,
                    },
                ));
            }
            while let Some(decoded) = self.prepared_disk.decoded_bytes(&key, need) {
                if self
                    .prepared_disk
                    .read_bound(&key, need)
                    .is_some_and(|bound| !cache_reads.admits(bound))
                {
                    self.prepared_disk.check_dir()?;
                    return Err(prepared_cache_read_limit());
                }
                let mut reservation = self.reserve_projection(context, decoded)?;
                #[cfg(test)]
                self.fact_lookups.fetch_add(1, Ordering::Relaxed);
                let mut read = crate::snapshot_prepared_disk::DiskRead::default();
                let lookup = self.prepared_disk.lookup(
                    &key,
                    &mut read,
                    need,
                    decoded,
                    cancel,
                    remaining.deadline_unix_ms,
                );
                usage[18] += read.operations;
                usage[19] += read.bytes;
                match lookup? {
                    crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                        let facts = Arc::new(facts);
                        return match self.cache_prepared_facts(
                            root.stamp,
                            Arc::clone(&facts),
                            classifier,
                            context,
                            &mut reservation,
                            need,
                        ) {
                            Ok(retained) => Ok((retained, reservation)),
                            Err(error) if error.status == Status::RetainedLimit => Ok((
                                self.retain_facts(&mut reservation, context, facts)?,
                                reservation,
                            )),
                            Err(error) => Err(error),
                        };
                    }
                    crate::snapshot_prepared_disk::DiskLookup::Retired => {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "prepared root facts revision was evicted",
                        ));
                    }
                    crate::snapshot_prepared_disk::DiskLookup::Changed => continue,
                    crate::snapshot_prepared_disk::DiskLookup::IndexOnly(compact) => {
                        drop(compact);
                        break;
                    }
                    crate::snapshot_prepared_disk::DiskLookup::Miss => break,
                }
            }
            drop(claim);
            let mut reservation = self.reserve_projection(context, bound)?;
            let mut fact_limits = *remaining;
            fact_limits.max_read_bytes = self.config.source;
            fact_limits.max_events = root.event_count;
            #[cfg(test)]
            self.fact_builds.fetch_add(1, Ordering::Relaxed);
            let (facts, _, _) = crate::snapshot_projection::prepare_facts(
                root,
                &json!([]),
                &fact_limits,
                cancel,
                need,
            )?;
            assert!(
                facts.accounted_bytes() <= bound,
                "prepared root facts outgrew their bound"
            );
            let facts = Arc::new(facts);
            #[cfg(test)]
            {
                if need == FactsNeed::Full {
                    let hook = self
                        .built_facts_hook
                        .lock()
                        .expect("built facts hook")
                        .take();
                    if let Some(hook) = hook {
                        hook(&facts);
                    }
                }
            }
            if self.prepared_disk.insert(
                &key,
                &facts,
                |bytes| {
                    self.extend_projection_reservation(&mut reservation, context, bytes)
                        .is_ok()
                },
                |growth| self.admit_prepared_disk_growth(context, growth),
            )? == crate::snapshot_prepared_disk::Inserted::AlreadyFull
            {
                drop(facts);
                drop(reservation);
                continue;
            }
            return match self.cache_prepared_facts(
                root.stamp,
                Arc::clone(&facts),
                classifier,
                context,
                &mut reservation,
                need,
            ) {
                Ok(retained) => Ok((retained, reservation)),
                Err(error) if error.status == Status::RetainedLimit => Ok((
                    self.retain_facts(&mut reservation, context, facts)?,
                    reservation,
                )),
                Err(error) => Err(error),
            };
        }
    }

    fn canonical_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        #[cfg(test)]
        self.source_resolutions.fetch_add(1, Ordering::Relaxed);
        std::fs::canonicalize(path)
    }

    fn prepared_source(
        &self,
        canonical: &Path,
        metadata: &std::fs::Metadata,
        context: &Value,
        remaining: &mut WorkLimits,
        cache_reads: PreparedCacheReads<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        need: FactsNeed,
    ) -> Result<(SourceStamp, PreparedSourceOutcome<'_>), SnapshotError> {
        cancel.check(remaining.deadline_unix_ms)?;
        self.authority(context, Some(canonical))?;
        if !metadata.is_file() {
            return Err(invalid("prepared graph source must be a file"));
        }
        let stamp = SourceStamp::of(metadata);
        {
            let mut state = self.lock_state();
            if let Some((slot, touched)) = state.prepared_loads.get_mut(&stamp.identity) {
                if slot.stamp == stamp {
                    slot.deadline.store(
                        now_ms().saturating_add(self.config.preparation),
                        Ordering::Release,
                    );
                    *touched = now_ms();
                } else {
                    state.prepared_loads.remove(&stamp.identity);
                    state.remove_load(&stamp.identity);
                }
            }
        }
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let classifier = json!({"id":"native","version":"1"});
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            registry_generation,
            admission,
            &context["authority"],
            &classifier,
        )?;
        loop {
        if let Some(retained) = self.touch_retained_facts(
            &key,
            stamp,
            registry_generation,
            admission,
            &context["authority"],
            &classifier,
            need,
        ) {
            usage[7] += 1;
            return Ok((
                stamp,
                PreparedSourceOutcome::Ready {
                    stamp,
                    held: HeldFacts::Retained(retained),
                    cached: true,
                },
            ));
        }
        let claim = self.claim_fact_read(cancel, remaining.deadline_unix_ms)?;
        if let Some(retained) = self.touch_retained_facts(
            &key,
            stamp,
            registry_generation,
            admission,
            &context["authority"],
            &classifier,
            need,
        ) {
            usage[7] += 1;
            return Ok((
                stamp,
                PreparedSourceOutcome::Ready {
                    stamp,
                    held: HeldFacts::Retained(retained),
                    cached: true,
                },
            ));
        }
        if let Some(decoded) = self.prepared_disk.decoded_bytes(&key, need) {
            if self
                .prepared_disk
                .read_bound(&key, need)
                .is_some_and(|bound| !cache_reads.admits(bound))
            {
                self.prepared_disk.check_dir()?;
                return Err(prepared_cache_read_limit());
            }
            if let PreparedCacheReads::Warming(_, membership, since) = cache_reads {
                if self.prepared_disk.touch(&key, need, |accounted| {
                    !self.lock_state().prepared_facts_fit_sparing(
                        key,
                        accounted,
                        self.prepared_fact_budget(),
                        |cached| {
                            membership
                                .active_member(cached.stamp.identity, since)
                                .is_some_and(|member| {
                                    cached.serves(
                                        member.stamp,
                                        registry_generation,
                                        admission,
                                        &context["authority"],
                                        &classifier,
                                        need,
                                    )
                                })
                        },
                    )
                })? {
                    return Ok((stamp, PreparedSourceOutcome::Stored));
                }
            }
            let mut reservation = self.reserve_projection(context, decoded)?;
            #[cfg(test)]
            self.fact_lookups.fetch_add(1, Ordering::Relaxed);
            let mut read = crate::snapshot_prepared_disk::DiskRead::default();
            let lookup = self.prepared_disk.lookup(
                &key,
                &mut read,
                need,
                decoded,
                cancel,
                remaining.deadline_unix_ms,
            );
            usage[18] += read.operations;
            usage[19] += read.bytes;
            if let PreparedCacheReads::Warming(..) = cache_reads {
                remaining.max_source_read_bytes = remaining
                    .max_source_read_bytes
                    .saturating_sub(read.bytes as usize);
            }
            match lookup? {
                crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                    usage[7] += 1;
                    let facts = Arc::new(facts);
                    let held = match self.cache_prepared_facts(
                        stamp,
                        Arc::clone(&facts),
                        &classifier,
                        context,
                        &mut reservation,
                        need,
                    ) {
                        Ok(retained) => HeldFacts::Retained(retained),
                        Err(error) if error.status == Status::RetainedLimit => {
                            HeldFacts::Reserved { facts, reservation }
                        }
                        Err(error) => return Err(error),
                    };
                    return Ok((
                        stamp,
                        PreparedSourceOutcome::Ready {
                            stamp,
                            held,
                            cached: false,
                        },
                    ));
                }
                crate::snapshot_prepared_disk::DiskLookup::Retired => {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "prepared facts revision was evicted",
                    ));
                }
                crate::snapshot_prepared_disk::DiskLookup::Changed => continue,
                crate::snapshot_prepared_disk::DiskLookup::IndexOnly(compact) => {
                    drop(compact);
                    drop(reservation);
                }
                crate::snapshot_prepared_disk::DiskLookup::Miss => {}
            }
        }
        drop(claim);
        let acquire = json!({"schema":SCHEMA,"id":"prepare-graph-source","operation":"acquire","path":canonical.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":remaining.deadline_unix_ms,"limits":remaining.to_json()});
        let before_bytes = usage[1];
        let before_events = usage[3];
        let outcome = self.acquire(&acquire, context, cancel, usage);
        {
            let mut state = self.lock_state();
            if let Some(slot) = state.loads.get(&stamp.identity).cloned() {
                let growth = state.prepared_load_growth(&stamp.identity);
                if self.admit_memory(&mut state, context, growth).is_ok() {
                    state.insert_prepared_load(stamp.identity, slot, now_ms());
                }
            }
        }
        let outcome = outcome?;
        remaining.max_source_read_bytes = remaining
            .max_source_read_bytes
            .saturating_sub((usage[1] - before_bytes) as usize);
        remaining.max_events = remaining
            .max_events
            .saturating_sub((usage[3] - before_events) as usize);
        if let Some(cursor) = outcome.1 {
            return Ok((stamp, PreparedSourceOutcome::Pending(cursor)));
        }
        match self.finish_prepared_source(stamp, &outcome.0, context, remaining, cancel, need)? {
            PreparedSourceOutcome::Superseded => continue,
            result => return Ok((stamp, result)),
        }
        }
    }

    fn resume_prepared_source(
        &self,
        pending: &PendingPreparedSource,
        dirs: &mut ResolvedDirs<'_>,
        context: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        need: FactsNeed,
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        self.authority(
            context,
            Some(&dirs.resolve(&pending.path)?.0),
        )?;
        self.advance_prepared_source(pending, context, remaining, cancel, usage, need)
    }

    fn advance_prepared_source(
        &self,
        pending: &PendingPreparedSource,
        context: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        need: FactsNeed,
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        let waiter = self
            .rebind_waiter(&mut self.lock_state(), &pending.token, context)?
            .ok_or_else(|| {
                SnapshotError::new(Status::StaleCursor, "prepared source reservation expired")
            })?;
        let before_bytes = usage[1];
        let before_events = usage[3];
        let outcome = self.advance(&pending.token, waiter, Some(remaining), cancel, usage)?;
        remaining.max_source_read_bytes = remaining
            .max_source_read_bytes
            .saturating_sub((usage[1] - before_bytes) as usize);
        remaining.max_events = remaining
            .max_events
            .saturating_sub((usage[3] - before_events) as usize);
        if let Some(cursor) = outcome.1 {
            Ok(PreparedSourceOutcome::Pending(cursor))
        } else {
            self.finish_prepared_source(pending.stamp, &outcome.0, context, remaining, cancel, need)
        }
    }

    fn finish_prepared_source(
        &self,
        stamp: SourceStamp,
        outcome: &Value,
        context: &Value,
        remaining: &WorkLimits,
        cancel: &Cancellation,
        need: FactsNeed,
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        let handle = &outcome["description"]["handle"];
        let pinned = self
            .renew_scope_for_work(handle, context, remaining.deadline_unix_ms)
            .and_then(|scope| {
                let snapshot = scope.snapshot;
                if snapshot.stamp == stamp {
                    Ok(snapshot)
                } else {
                    Err(SnapshotError::new(
                        Status::Changed,
                        "prepared source revision changed",
                    ))
                }
            });
        if pinned.is_err() {
            self.lock_state()
                .leases
                .remove(str_field(handle, "lease_id")?);
        }
        let snapshot = pinned?;
        let mut fact_limits = *remaining;
        fact_limits.max_read_bytes = self.config.source;
        fact_limits.max_events = snapshot.event_count;
        let bound = crate::snapshot_projection::facts_bound(&snapshot, need);
        let prepared = self
            .reserve_projection(context, bound)
            .and_then(|reservation| {
                #[cfg(test)]
                self.fact_builds.fetch_add(1, Ordering::Relaxed);
                crate::snapshot_projection::prepare_facts(
                    &snapshot,
                    &json!([]),
                    &fact_limits,
                    cancel,
                    need,
                )
                .map(|(facts, _, _)| {
                    assert!(
                        facts.accounted_bytes() <= bound,
                        "prepared source facts outgrew their bound"
                    );
                    (reservation, facts)
                })
            });
        {
            let mut state = self.lock_state();
            state.leases.remove(str_field(handle, "lease_id")?);
            let recent_codex = snapshot.provider == Provider::Codex
                && snapshot.codex_raw.is_some()
                && (now_ms() as i128 * 1_000_000).saturating_sub(stamp.mtime_ns)
                    <= 30 * 60 * 1_000_000_000;
            let growth = state.recent_codex_growth(&stamp.identity);
            if recent_codex && self.admit_memory(&mut state, context, growth).is_ok() {
                state.insert_recent_codex(stamp.identity, now_ms());
                while state.recent_codex_raw_bytes > 128 * 1024 * 1024 {
                    let oldest = state
                        .recent_codex
                        .iter()
                        .min_by_key(|(_, touched)| *touched)
                        .map(|(identity, _)| *identity)
                        .expect("raw codex cache exceeds bound");
                    state.remove_recent_codex(&oldest);
                    state.remove_latest(&oldest);
                }
            } else if snapshot.provider == Provider::Codex {
                state.remove_recent_codex(&stamp.identity);
                state.remove_latest(&stamp.identity);
            }
        }
        let (mut reservation, facts) = prepared?;
        let facts = Arc::new(facts);
        #[cfg(test)]
        {
            let hook = self
                .built_facts_hook
                .lock()
                .expect("built facts hook")
                .take();
            if let Some(hook) = hook {
                hook(&facts);
            }
        }
        drop(snapshot);
        let classifier = json!({"id":"native","version":"1"});
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            str_field(context, "registry_generation")?,
            str_field(context, "admission")?,
            &context["authority"],
            &classifier,
        )?;
        if self.prepared_disk.insert(
            &key,
            &facts,
            |bytes| {
                self.extend_projection_reservation(&mut reservation, context, bytes)
                    .is_ok()
            },
            |growth| self.admit_prepared_disk_growth(context, growth),
        )? == crate::snapshot_prepared_disk::Inserted::AlreadyFull
        {
            drop(facts);
            drop(reservation);
            self.lock_state()
                .prepared_loads
                .remove(&stamp.identity);
            return Ok(PreparedSourceOutcome::Superseded);
        }
        let held = match self.cache_prepared_facts(
            stamp,
            Arc::clone(&facts),
            &classifier,
            context,
            &mut reservation,
            need,
        ) {
            Ok(retained) => HeldFacts::Retained(retained),
            Err(error) if error.status == Status::RetainedLimit => {
                HeldFacts::Reserved { facts, reservation }
            }
            Err(error) => return Err(error),
        };
        self.lock_state()
            .prepared_loads
            .remove(&stamp.identity);
        Ok(PreparedSourceOutcome::Ready {
            stamp,
            held,
            cached: false,
        })
    }

    fn warm_membership_key(
        request: &Value,
        context: &Value,
    ) -> Result<String, SnapshotError> {
        let binding = json!({
            "thread_ids": request["thread_ids"],
            "roots": request["roots"],
            "direct_paths": request["direct_paths"],
            "active_since_unix_ms": request["active_since_unix_ms"],
            "registry_generation": context["registry_generation"],
            "admission": context["admission"],
            "authority": context["authority"],
        });
        let canonical = crate::ids::canonical_json(&binding)
            .map_err(|error| invalid(error.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
    }

    fn validate_warm_membership(
        &self,
        members: &[PreparedSourceRef],
        sidechain_dirs: &[(PathBuf, Option<SourceStamp>)],
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<bool, SnapshotError> {
        Ok(self.validate_source_stamps(members, context, cancel, deadline)?
            && self.validate_sidechain_dirs(sidechain_dirs, context, cancel, deadline)?)
    }

    fn validate_source_stamps(
        &self,
        sources: &[PreparedSourceRef],
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<bool, SnapshotError> {
        let mut dirs = ResolvedDirs::new(self, context);
        for source in sources {
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
            cancel.check(deadline)?;
            let (canonical, metadata) = match dirs.resolve(&source.path) {
                Ok(resolved) => resolved,
                Err(error) if error.status == Status::Missing => return Ok(false),
                Err(error) => return Err(error),
            };
            self.authority(context, Some(&canonical))?;
            if canonical != source.path || SourceStamp::of(&metadata) != source.stamp {
                return Ok(false);
            }
        }
        dirs.hold(|| cancel.check(deadline))
    }

    fn validate_sidechain_dirs(
        &self,
        sidechain_dirs: &[(PathBuf, Option<SourceStamp>)],
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<bool, SnapshotError> {
        let mut dirs = ResolvedDirs::new(self, context);
        for (path, stamp) in sidechain_dirs {
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
            cancel.check(deadline)?;
            let (canonical, metadata) = match dirs.resolve(path) {
                Ok(resolved) => resolved,
                Err(error) if error.status == Status::Missing => {
                    if stamp.is_some() {
                        return Ok(false);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.authority(context, Some(&canonical))?;
            if stamp.is_none_or(|stamp| SourceStamp::of(&metadata) != stamp) {
                return Ok(false);
            }
        }
        dirs.hold(|| cancel.check(deadline))
    }

    fn prepare_graph_ready(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let remaining = limits(request)?;
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        if !view["attachments"]
            .as_array()
            .is_some_and(|attachments| attachments.is_empty())
        {
            return Err(invalid(
                "prepare_graph accepts registry identities, not attachments",
            ));
        }
        let root_handle = view
            .get("handle")
            .ok_or_else(|| invalid("missing root handle"))?;
        let scope = self.renew_scope_for_work(root_handle, context, remaining.deadline_unix_ms)?;
        let root = scope.snapshot;
        if !classifier_eq(&view["classifier"], &scope.classifier)? {
            return Err(invalid("prepared graph classifier differs from root"));
        }
        let ids = request
            .get("thread_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing thread_ids"))?;
        let roots = request
            .get("roots")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing roots"))?;
        request
            .get("direct_paths")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing direct_paths"))?;
        if roots.len() > 64 {
            return Err(invalid("prepared registry input exceeds its bound"));
        }
        let since = active_since_ns(request)?;
        if !ids.is_empty() {
            let key = Self::warm_membership_key(request, context)?;
            let mut membership_limits = remaining;
            let (membership, mut membership_reservation) = self.registered_membership(
                &key,
                request,
                context,
                cancel,
                usage,
                &mut membership_limits,
                true,
            )?;
            self.publish_warm_membership(
                &key,
                &membership,
                context,
                membership_reservation.as_mut(),
            )?;
            drop(membership_reservation);
            let members = Arc::clone(&membership.members);
            let sidechain_dirs = Arc::clone(&membership.sidechain_dirs);
            let (retained, mut reservation) = self.prepared_root_facts(
                &root,
                &view["classifier"],
                context,
                &remaining,
                PreparedCacheReads::Unmetered,
                cancel,
                usage,
                FactsNeed::Index,
            )?;
            let shared = members
                .iter()
                .position(|source| source.stamp.identity.file() == root.stamp.identity.file());
            let (before, after) = match shared {
                Some(index) => (&members[..index], &members[index + 1..]),
                None => (&members[..], &members[members.len()..]),
            };
            let others = || {
                before
                    .iter()
                    .chain(after)
                    .filter(move |source| active(source, since))
            };
            let copied = shared.is_some() || since.is_some();
            let graph_id = self.token("prepared-graph");
            let stamps_bytes = (others().count() + 1) * size_of::<(PathBuf, SourceStamp)>()
                + root.canonical_path.as_os_str().len()
                + others()
                    .map(|source| source.path.as_os_str().len())
                    .sum::<usize>();
            let buffers: usize = if copied {
                arc_slice_bytes::<PreparedSourceRef>(others().count())
                    + others()
                        .map(|source| source.path.as_os_str().len())
                        .sum::<usize>()
            } else {
                0
            };
            let record = str_field(context, "claimant")?.len()
                + str_field(context, "registry_generation")?.len()
                + str_field(context, "admission")?.len()
                + value_bytes(&context["authority"])
                + value_bytes(root_handle)
                + value_bytes(&view["classifier"])
                + 2 * <Sha256 as Digest>::output_size();
            self.extend_projection_reservation(
                &mut reservation,
                context,
                PreparedGraph::key_charge(&graph_id)
                    + PREPARED_GRAPH_BYTES
                    + record
                    + stamps_bytes
                    + buffers,
            )?;
            let sources: Arc<[PreparedSourceRef]> = if copied {
                #[cfg(test)]
                self.registered_sources.fetch_add(1, Ordering::Relaxed);
                others().cloned().collect()
            } else {
                Arc::clone(&members)
            };
            let mut stamps = Vec::with_capacity(sources.len() + 1);
            stamps.push((root.canonical_path.clone(), root.stamp));
            stamps.extend(
                sources
                    .iter()
                    .map(|source| (source.path.clone(), source.stamp)),
            );
            let mut digest = Sha256::new();
            for (path, stamp) in &stamps {
                digest.update(path.as_os_str().as_encoded_bytes());
                digest.update(stamp.revision().as_bytes());
            }
            let revision = format!("{:x}", digest.finalize());
            let work = self.lock_state().ledger.shared.work().clone();
            let graph = PreparedGraph {
                claimant: str_field(context, "claimant")?.to_owned(),
                registry_generation: str_field(context, "registry_generation")?.to_owned(),
                admission: str_field(context, "admission")?.to_owned(),
                authority: context["authority"].clone(),
                root,
                root_handle: root_handle.clone(),
                classifier: view["classifier"].clone(),
                root_facts: Arc::clone(retained.facts()),
                root_slices: Table::new(work),
                revision: revision.clone(),
                stamps,
                validated: false,
                sources,
                sidechain_dirs,
                remaining,
                expires: (now_ms() + self.config.ttl).min(remaining.deadline_unix_ms),
            };
            return self.publish_prepared_graph(
                graph_id,
                graph,
                revision,
                context,
                &mut reservation,
            );
        }
        let (retained, mut reservation) = self.prepared_root_facts(
            &root,
            &view["classifier"],
            context,
            &remaining,
            PreparedCacheReads::Unmetered,
            cancel,
            usage,
            FactsNeed::Index,
        )?;
        let seen_capacity = set_capacity_for(&HashSet::<SourceIdentity>::new(), 1);
        self.extend_projection_reservation(
            &mut reservation,
            context,
            size_of::<PreparedBuild>()
                + str_field(context, "claimant")?.len()
                + value_bytes(context)
                + value_bytes(request)
                + value_bytes(root_handle)
                + value_bytes(&view["classifier"])
                + set_growth(&HashSet::<SourceIdentity>::new(), 1)
                + size_of::<(PathBuf, SourceStamp)>()
                + root.canonical_path.as_os_str().len(),
        )?;
        #[cfg(test)]
        self.build_records.fetch_add(1, Ordering::Relaxed);
        let mut build = PreparedBuild {
            charge_work: Work::default(),
            owned_payload_bytes: 0,
            context_bytes: 0,
            claimant: str_field(context, "claimant")?.to_owned(),
            context: context.clone(),
            request: request.clone(),
            root: Arc::clone(&root),
            root_handle: root_handle.clone(),
            classifier: view["classifier"].clone(),
            root_facts: Arc::clone(retained.facts()),
            remaining,
            tasks: Vec::new(),
            listing: None,
            seen: HashSet::from([root.stamp.identity.file()]),
            sources: Vec::new(),
            stamps: vec![(root.canonical_path.clone(), root.stamp)],
            sidechain_dirs: Vec::new(),
            expires: (now_ms() + self.config.ttl).min(remaining.deadline_unix_ms),
        };
        build.context_bytes = value_bytes(&build.context);
        build.owned_payload_bytes = build.claimant.capacity()
            + build.context_bytes
            + value_bytes(&build.request)
            + value_bytes(&build.root_handle)
            + value_bytes(&build.classifier)
            + build.stamps[0].0.capacity();
        build.charge_work.tick(1);
        assert_eq!(
            (build.seen.capacity(), build.stamps.capacity()),
            (seen_capacity, 1),
            "prepared build collections landed off their predicted capacities"
        );
        self.prepare_graph_step(
            &self.token("prepared-build"),
            build,
            &mut reservation,
            cancel,
            usage,
        )
    }

    fn validate_prepared_build_scope(
        &self,
        build: &PreparedBuild,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        cancel.check(build.remaining.deadline_unix_ms)?;
        let context = &build.context;
        let scope = self.renew_scope_for_work(
            &build.root_handle,
            context,
            build.remaining.deadline_unix_ms,
        )?;
        let root = scope.snapshot;
        if root.stamp != build.root.stamp || !classifier_eq(&build.classifier, &scope.classifier)? {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared graph root generation changed",
            ));
        }
        Ok(())
    }

    fn prepare_graph_step(
        &self,
        token: &str,
        mut build: PreparedBuild,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        self.validate_prepared_build_scope(&build, cancel)?;
        let context = &build.context;
        if build.tasks.is_empty() && build.sources.is_empty() && build.listing.is_none() {
            let direct = build.request["direct_paths"]
                .as_array()
                .expect("validated direct paths");
            let direct_bytes = direct
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(str::len)
                        .ok_or_else(|| invalid("invalid direct path"))
                })
                .sum::<Result<usize, SnapshotError>>()?;
            self.extend_projection_reservation(
                reservation,
                context,
                direct.len() * size_of::<PathBuf>()
                    + set_growth(&HashSet::<&str>::new(), direct.len())
                    + direct_bytes
                    + vec_growth(&build.tasks, direct.len() + 1)
                    + build.root.canonical_path.as_os_str().len(),
            )?;
            let predicted = (
                vec_capacity_for(&build.tasks, direct.len() + 1),
                set_capacity_for(&HashSet::<&str>::new(), direct.len()),
            );
            build.tasks.reserve(direct.len() + 1);
            let mut distinct = Vec::with_capacity(direct.len());
            let mut paths = HashSet::with_capacity(direct.len());
            assert_eq!(
                (build.tasks.capacity(), paths.capacity()),
                predicted,
                "prepared build collections landed off their predicted capacities"
            );
            for path in direct {
                let path = path.as_str().expect("validated direct path");
                if paths.insert(path) {
                    distinct.push(PathBuf::from(path));
                }
            }
            for path in distinct.into_iter().rev() {
                build.owned_payload_bytes += path.capacity();
                build.charge_work.tick(1);
                build.tasks.push(GraphTask::Visit {
                    path,
                    depth: 1,
                    spawned_by: None,
                });
            }
            let parent = build.root.canonical_path.clone();
            build.owned_payload_bytes += parent.capacity();
            build.charge_work.tick(1);
            build.tasks.push(GraphTask::List { parent, depth: 1 });
        }
        self.extend_projection_reservation(reservation, context, FILESYSTEM_PATH_BYTES)?;
        let work_stop = self.config.event_step.min(self.config.page_items);
        let mut examined = 0usize;
        while examined < work_stop {
            if examined != 0 {
                self.validate_prepared_build_scope(&build, cancel)?;
            }
            let checkpoint = examined.saturating_add(8).min(work_stop);
            while examined < checkpoint {
                cancel.check(build.remaining.deadline_unix_ms)?;
                if let Some(mut listing) = build.listing.take() {
                    let mut complete = false;
                    while examined < checkpoint {
                        cancel.check(build.remaining.deadline_unix_ms)?;
                        let Some(entry) = listing.entries.next() else {
                            complete = true;
                            break;
                        };
                        if build.remaining.max_discovery_entries == 0 {
                            return Err(SnapshotError::new(
                                Status::Incomplete,
                                "prepared graph discovery budget exhausted",
                            ));
                        }
                        build.remaining.max_discovery_entries -= 1;
                        usage[17] += 1;
                        examined += 1;
                        let entry = entry.map_err(io_error)?;
                        let path = entry.path();
                        if path
                            .extension()
                            .is_some_and(|extension| extension == "jsonl")
                            && !path
                                .file_name()
                                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
                        {
                            self.extend_projection_reservation(
                                reservation,
                                context,
                                path.capacity() + vec_growth(&listing.children, 1),
                            )?;
                            let predicted = vec_capacity_for(&listing.children, 1);
                            build.owned_payload_bytes += path.capacity();
                            build.charge_work.tick(1);
                            listing.children.push(path);
                            assert_eq!(
                                listing.children.capacity(),
                                predicted,
                                "prepared build collections landed off their predicted capacities"
                            );
                        }
                    }
                    if !complete {
                        build.listing = Some(listing);
                        break;
                    }
                    listing.children.sort_unstable();
                    self.extend_projection_reservation(
                        reservation,
                        context,
                        vec_growth(&build.tasks, listing.children.len())
                            + listing
                                .children
                                .iter()
                                .map(|path| spawner(path).len())
                                .sum::<usize>(),
                    )?;
                    let predicted = vec_capacity_for(&build.tasks, listing.children.len());
                    build.tasks.reserve(listing.children.len());
                    assert_eq!(
                        build.tasks.capacity(),
                        predicted,
                        "prepared build collections landed off their predicted capacities"
                    );
                    for path in listing.children.into_iter().rev() {
                        let spawned_by = spawner(&path).to_owned();
                        build.owned_payload_bytes += spawned_by.capacity();
                        build.charge_work.tick(1);
                        build.tasks.push(GraphTask::Visit {
                            path,
                            depth: listing.depth,
                            spawned_by: Some(spawned_by),
                        });
                    }
                    continue;
                }
                let Some(task) = build.tasks.pop() else {
                    break;
                };
                build.owned_payload_bytes -= task_bytes(&task);
                build.charge_work.tick(1);
                match task {
                    GraphTask::List { parent, depth } => {
                        let base = parent
                            .parent()
                            .ok_or_else(|| invalid("source has no parent"))?;
                        let stem = parent
                            .file_stem()
                            .ok_or_else(|| invalid("source has no stem"))?;
                        let directory_bytes = sidechain_directory_capacity(base, stem);
                        self.extend_projection_reservation(
                            reservation,
                            context,
                            directory_bytes + vec_growth(&build.sidechain_dirs, 1),
                        )?;
                        let predicted = vec_capacity_for(&build.sidechain_dirs, 1);
                        let directory = sidechain_directory(base, stem);
                        assert_eq!(
                            directory.capacity(),
                            directory_bytes,
                            "sidechain directory landed off its predicted capacity"
                        );
                        let canonical = match realpath(&directory) {
                            Ok(canonical) => canonical,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                build.owned_payload_bytes += directory.capacity();
                                build.charge_work.tick(1);
                                build.sidechain_dirs.push((directory, None));
                                assert_eq!(
                                    build.sidechain_dirs.capacity(),
                                    predicted,
                                    "prepared build collections landed off their predicted capacities"
                                );
                                continue;
                            }
                            Err(error) => return Err(io_error(error)),
                        };
                        self.authority(context, Some(&canonical))?;
                        let stamp =
                            SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
                        self.extend_projection_reservation(
                            reservation,
                            context,
                            canonical.capacity()
                                + READ_DIR_HANDLE_BYTES
                                + directory.as_os_str().len(),
                        )?;
                        build.owned_payload_bytes += canonical.capacity();
                        build.charge_work.tick(1);
                        build.sidechain_dirs.push((canonical, Some(stamp)));
                        assert_eq!(
                            build.sidechain_dirs.capacity(),
                            predicted,
                            "prepared build collections landed off their predicted capacities"
                        );
                        build.listing = Some(GraphListing {
                            entries: OpenDirectory::open(&directory)?,
                            children: Vec::new(),
                            depth,
                        });
                        #[cfg(test)]
                        self.directory_opens.fetch_add(1, Ordering::Relaxed);
                    }
                    GraphTask::Visit {
                        path,
                        depth,
                        spawned_by: _,
                    } => {
                        examined += 1;
                        let canonical = realpath(&path).map_err(io_error)?;
                        self.authority(context, Some(&canonical))?;
                        let stamp =
                            SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
                        if build.seen.contains(&stamp.identity.file()) {
                            continue;
                        }
                        if active_since_ns(&build.request)?
                            .is_some_and(|since| stamp.mtime_ns < since)
                        {
                            self.extend_projection_reservation(
                                reservation,
                                context,
                                set_growth(&build.seen, 1) + canonical.as_os_str().len(),
                            )?;
                            let predicted =
                                (set_capacity_for(&build.seen, 1), build.tasks.capacity());
                            build.seen.insert(stamp.identity.file());
                            build.owned_payload_bytes += canonical.capacity();
                            build.charge_work.tick(1);
                            build.tasks.push(GraphTask::List {
                                parent: canonical,
                                depth: depth + 1,
                            });
                            assert_eq!(
                                (build.seen.capacity(), build.tasks.capacity()),
                                predicted,
                                "prepared build collections landed off their predicted capacities"
                            );
                            continue;
                        }
                        if build.stamps.len() >= build.remaining.max_sources {
                            return Err(SnapshotError::new(
                                Status::Incomplete,
                                "prepared graph source budget exhausted",
                            ));
                        }
                        self.extend_projection_reservation(
                            reservation,
                            context,
                            set_growth(&build.seen, 1)
                                + vec_growth(&build.stamps, 1)
                                + vec_growth(&build.sources, 1)
                                + 3 * canonical.as_os_str().len(),
                        )?;
                        let predicted = (
                            set_capacity_for(&build.seen, 1),
                            vec_capacity_for(&build.stamps, 1),
                            vec_capacity_for(&build.sources, 1),
                            build.tasks.capacity(),
                        );
                        #[cfg(test)]
                        self.build_sources.fetch_add(1, Ordering::Relaxed);
                        build.seen.insert(stamp.identity.file());
                        build.stamps.push((canonical.clone(), stamp));
                        build.sources.push(PreparedSourceRef {
                            path: canonical.clone(),
                            stamp,
                        });
                        build.owned_payload_bytes += build.stamps.last().unwrap().0.capacity()
                            + build.sources.last().unwrap().path.capacity()
                            + canonical.capacity();
                        build.charge_work.tick(3);
                        build.tasks.push(GraphTask::List {
                            parent: canonical,
                            depth: depth + 1,
                        });
                        assert_eq!(
                            (
                                build.seen.capacity(),
                                build.stamps.capacity(),
                                build.sources.capacity(),
                                build.tasks.capacity(),
                            ),
                            predicted,
                            "prepared build collections landed off their predicted capacities"
                        );
                    }
                }
            }
            if build.listing.is_none() && build.tasks.is_empty() {
                break;
            }
        }
        if build.listing.is_some() || !build.tasks.is_empty() {
            return self.store_prepared_build(token, build, reservation);
        }
        let graph_id = self.token("prepared-graph");
        self.extend_projection_reservation(
            reservation,
            context,
            PreparedGraph::key_charge(&graph_id)
                + PREPARED_GRAPH_BYTES
                + str_field(context, "registry_generation")?.len()
                + str_field(context, "admission")?.len()
                + value_bytes(&context["authority"])
                + 2 * <Sha256 as Digest>::output_size()
                + arc_slice_bytes::<PreparedSourceRef>(build.sources.len())
                + arc_slice_bytes::<(PathBuf, Option<SourceStamp>)>(build.sidechain_dirs.len()),
        )?;
        let mut digest = Sha256::new();
        for (path, stamp) in &build.stamps {
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update(stamp.revision().as_bytes());
        }
        let revision = format!("{:x}", digest.finalize());
        let work = self.lock_state().ledger.shared.work().clone();
        let graph = PreparedGraph {
            claimant: build.claimant,
            registry_generation: str_field(context, "registry_generation")?.to_owned(),
            admission: str_field(context, "admission")?.to_owned(),
            authority: context["authority"].clone(),
            root: build.root,
            root_handle: build.root_handle,
            classifier: build.classifier,
            root_facts: build.root_facts,
            root_slices: Table::new(work),
            revision: revision.clone(),
            stamps: build.stamps,
            validated: false,
            sources: build.sources.into(),
            sidechain_dirs: build.sidechain_dirs.into(),
            remaining: build.remaining,
            expires: (now_ms() + self.config.ttl).min(build.remaining.deadline_unix_ms),
        };
        self.publish_prepared_graph(graph_id, graph, revision, context, reservation)
    }

    fn publish_prepared_graph(
        &self,
        graph_id: String,
        graph: PreparedGraph,
        revision: String,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut state = self.lock_state();
        Self::prune(&mut state);
        if state.prepared_graphs.len() >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared graph admission exhausted",
            ));
        }
        let record =
            charged_bytes(&graph_id, &graph) + state.ledger.shared.unowned_bytes(graph.anchors());
        let covered = record.min(reservation.bytes);
        let fresh = record - covered
            + state.ledger.shared.growth(graph.anchors())
            + state.prepared_graphs.growth_for(&graph_id);
        self.admit_memory(&mut state, context, fresh)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_prepared_graph(graph_id.clone(), Arc::new(Mutex::new(graph)));
        Ok((
            json!({"kind":"prepared_graph","handle":{"graph_id":graph_id,"owner_epoch":self.owner_epoch,"revision":revision,"complete":true}}),
            None,
            None,
        ))
    }

    fn store_prepared_build(
        &self,
        token: &str,
        mut build: PreparedBuild,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut state = self.lock_state();
        if state.prepared_builds.len() + state.registered_builds.len() >= self.lease_cap(&build.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared build admission exhausted",
            ));
        }
        build.expires = (now_ms() + self.config.ttl).min(build.remaining.deadline_unix_ms);
        let token = token.to_owned();
        let pledge = Delivery::cursor_pledge(&build.claimant, &token);
        let additional = state.admission(&token, &build, [facts_anchor(&build.root_facts)])
            + state.prepared_builds.growth_for(&token)
            + pledge;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, &build.context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_prepared_build(token.clone(), build);
        state.prepared_builds.pledge(&token, pledge);
        Ok((
            Value::new_null(),
            Some(token),
            Some("prepared graph work incomplete".to_owned()),
        ))
    }

    fn registered_membership(
        &self,
        key: &str,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        remaining: &mut WorkLimits,
        revalidate: bool,
    ) -> Result<(WarmMembership, Option<ProjectionReservation<'_>>), SnapshotError> {
        let cached = self.lock_state().warm_memberships.get(key).cloned();
        if let Some(cached) = cached {
            if !revalidate
                || self.validate_warm_membership(
                    &cached.members,
                    &cached.sidechain_dirs,
                    context,
                    cancel,
                    remaining.deadline_unix_ms,
                )?
            {
                return Ok((cached, None));
            }
            self.lock_state().remove_warm_membership(key);
            if let Some((membership, reservation)) =
                self.refreshed_warm_membership(key, &cached, request, context, cancel, remaining)?
            {
                return Ok((membership, Some(reservation)));
            }
        }
        let (membership, reservation) =
            self.build_warm_membership(key, request, context, cancel, usage, remaining)?;
        if !membership.complete {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered membership is incomplete",
            ));
        }
        Ok((membership, Some(reservation)))
    }

    fn refreshed_warm_membership(
        &self,
        key: &str,
        cached: &WarmMembership,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        remaining: &WorkLimits,
    ) -> Result<Option<(WarmMembership, ProjectionReservation<'_>)>, SnapshotError> {
        if cached.expires <= now_ms()
            || !self.validate_sidechain_dirs(
                &cached.sidechain_dirs,
                context,
                cancel,
                remaining.deadline_unix_ms,
            )?
        {
            return Ok(None);
        }
        let mut reservation = self.reserve_projection(
            context,
            key.len()
                + cached.accounted_bytes()
                + cached.members.len() * size_of::<PreparedSourceRef>()
                + source_ref_bytes(&cached.members)
                + arc_slice_bytes::<(SourceIdentity, usize)>(cached.positions.len())
                + sidechain_dir_bytes(&cached.sidechain_dirs),
        )?;
        let mut dirs = ResolvedDirs::new(self, context);
        let mut members = Vec::with_capacity(cached.members.len());
        for member in cached.members.iter() {
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
            cancel.check(remaining.deadline_unix_ms)?;
            let (canonical, metadata) = match dirs.resolve(&member.path) {
                Ok(resolved) => resolved,
                Err(error) if error.status == Status::Missing => return Ok(None),
                Err(error) => return Err(error),
            };
            self.authority(context, Some(&canonical))?;
            let stamp = SourceStamp::of(&metadata);
            if canonical != member.path
                || stamp.identity != member.stamp.identity
                || (stamp != member.stamp && stamp.size <= member.stamp.size)
            {
                return Ok(None);
            }
            self.extend_projection_reservation(&mut reservation, context, canonical.capacity())?;
            members.push(PreparedSourceRef {
                path: canonical,
                stamp,
            });
        }
        let since = active_since_ns(request)?;
        if !dirs.hold(|| cancel.check(remaining.deadline_unix_ms))?
            || members
                .iter()
                .filter(|member| active(member, since))
                .count()
                > remaining.max_sources
        {
            return Ok(None);
        }
        self.extend_projection_reservation(
            &mut reservation,
            context,
            arc_slice_bytes::<PreparedSourceRef>(members.len()),
        )?;
        Ok(Some((
            WarmMembership {
                revision: Self::warm_membership_revision(&members),
                members: members.into(),
                positions: Arc::clone(&cached.positions),
                sidechain_dirs: Arc::clone(&cached.sidechain_dirs),
                complete: cached.complete,
                expires: cached.expires,
            },
            reservation,
        )))
    }

    fn warm_membership_revision(members: &[PreparedSourceRef]) -> String {
        use std::fmt::Write as _;

        let revision_len = 2 * <Sha256 as Digest>::output_size();
        let mut digest = Sha256::new();
        for source in members {
            digest.update(source.path.as_os_str().as_encoded_bytes());
            digest.update(source.stamp.revision().as_bytes());
        }
        let mut revision = String::with_capacity(revision_len);
        write!(revision, "{:x}", digest.finalize()).expect("hex digest");
        assert_eq!(revision.capacity(), revision_len);
        revision
    }

    fn build_warm_membership(
        &self,
        key: &str,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        remaining: &mut WorkLimits,
    ) -> Result<(WarmMembership, ProjectionReservation<'_>), SnapshotError> {
        let ids = request["thread_ids"]
            .as_array()
            .ok_or_else(|| invalid("missing registered thread ids"))?;
        let roots = request["roots"]
            .as_array()
            .ok_or_else(|| invalid("missing registered roots"))?;
        let direct = request["direct_paths"]
            .as_array()
            .ok_or_else(|| invalid("missing registered direct paths"))?;
        let revision_len = 2 * <Sha256 as Digest>::output_size();
        let mut reservation = self.reserve_projection(
            context,
            key.len() + size_of::<WarmMembership>() + revision_len,
        )?;
        #[cfg(test)]
        self.warm_records.fetch_add(1, Ordering::Relaxed);
        let since = active_since_ns(request)?;
        let (located, _catalog_reservation) = if ids.is_empty() {
            (HashMap::new(), None)
        } else {
            match self.catalog_access(
                CatalogInput { ids, roots }, context, cancel, remaining, usage, CatalogMode::Locate,
            )? {
                CatalogResolution::Ready { paths, reservation } => (paths, Some(reservation)),
                CatalogResolution::Indexed | CatalogResolution::Pending(_) => unreachable!("synchronous catalog lookup"),
            }
        };
        let mut members = Vec::new();
        let mut seen = HashSet::new();
        for path in ids
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|id| located.get(id).cloned())
            .chain(direct.iter().filter_map(Value::as_str).map(PathBuf::from))
        {
            cancel.check(remaining.deadline_unix_ms)?;
            let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
            self.authority(context, Some(&canonical))?;
            let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
            if !metadata.is_file() {
                return Err(invalid("registered source must be a file"));
            }
            let stamp = SourceStamp::of(&metadata);
            if seen.insert(stamp.identity) {
                self.extend_projection_reservation(
                    &mut reservation,
                    context,
                    vec_growth(&members, 1) + canonical.capacity(),
                )?;
                let predicted = vec_capacity_for(&members, 1);
                members.push(PreparedSourceRef {
                    path: canonical,
                    stamp,
                });
                assert_eq!(members.capacity(), predicted);
            }
        }
        let mut sidechain_dirs = Vec::new();
        let mut examined = 0usize;
        while examined < members.len() {
            cancel.check(remaining.deadline_unix_ms)?;
            let source = &members[examined];
            let directory = sidechain_directory(
                source
                    .path
                    .parent()
                    .ok_or_else(|| invalid("registered source has no parent"))?,
                source
                    .path
                    .file_stem()
                    .ok_or_else(|| invalid("registered source has no stem"))?,
            );
            let canonical = match std::fs::canonicalize(&directory) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        vec_growth(&sidechain_dirs, 1) + directory.capacity(),
                    )?;
                    let predicted = vec_capacity_for(&sidechain_dirs, 1);
                    sidechain_dirs.push((directory, None));
                    assert_eq!(sidechain_dirs.capacity(), predicted);
                    examined += 1;
                    continue;
                }
                Err(error) => return Err(io_error(error)),
            };
            self.authority(context, Some(&canonical))?;
            let directory_stamp = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
            self.extend_projection_reservation(
                &mut reservation,
                context,
                vec_growth(&sidechain_dirs, 1) + canonical.as_os_str().len(),
            )?;
            let predicted = vec_capacity_for(&sidechain_dirs, 1);
            sidechain_dirs.push((canonical.clone(), Some(directory_stamp)));
            assert_eq!(sidechain_dirs.capacity(), predicted);
            let mut children = Vec::new();
            for entry in std::fs::read_dir(&canonical).map_err(io_error)? {
                cancel.check(remaining.deadline_unix_ms)?;
                if remaining.max_discovery_entries == 0 {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "registered sidechain discovery budget exhausted",
                    ));
                }
                remaining.max_discovery_entries -= 1;
                usage[17] += 1;
                let entry = entry.map_err(io_error)?;
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "jsonl")
                    && !entry.file_name().to_string_lossy().starts_with("._")
                {
                    children.push(path);
                }
            }
            children.sort();
            for path in children {
                cancel.check(remaining.deadline_unix_ms)?;
                let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
                self.authority(context, Some(&canonical))?;
                let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                if !metadata.is_file() {
                    return Err(invalid("registered sidechain must be a file"));
                }
                let stamp = SourceStamp::of(&metadata);
                if seen.insert(stamp.identity) {
                    self.extend_projection_reservation(
                        &mut reservation,
                        context,
                        vec_growth(&members, 1) + canonical.capacity(),
                    )?;
                    let predicted = vec_capacity_for(&members, 1);
                    members.push(PreparedSourceRef {
                        path: canonical,
                        stamp,
                    });
                    assert_eq!(members.capacity(), predicted);
                }
            }
            examined += 1;
        }
        if members.iter().filter(|member| active(member, since)).count() > remaining.max_sources {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered source bound exhausted",
            ));
        }
        self.extend_projection_reservation(
            &mut reservation,
            context,
            arc_slice_bytes::<PreparedSourceRef>(members.len())
                + arc_slice_bytes::<(SourceIdentity, usize)>(members.len())
                + arc_slice_bytes::<(PathBuf, Option<SourceStamp>)>(sidechain_dirs.len()),
        )?;
        Ok((
            WarmMembership {
                revision: Self::warm_membership_revision(&members),
                positions: member_positions(&members),
                members: members.into(),
                sidechain_dirs: sidechain_dirs.into(),
                complete: located.len() == ids.len(),
                expires: now_ms().saturating_add(30 * 60_000),
            },
            reservation,
        ))
    }

    fn publish_warm_membership(
        &self,
        key: &String,
        membership: &WarmMembership,
        context: &Value,
        reservation: Option<&mut ProjectionReservation<'_>>,
    ) -> Result<(), SnapshotError> {
        let mut state = self.lock_state();
        Self::prune(&mut state);
        if state.warm_memberships.contains_key(key) {
            return Ok(());
        }
        if state.warm_memberships.len() >= 32 {
            let oldest = state
                .warm_memberships
                .iter()
                .min_by_key(|(_, membership)| membership.expires)
                .map(|(key, _)| key.clone())
                .expect("full warm membership cache");
            state.remove_warm_membership(&oldest);
        }
        let additional = state.admission(key, membership, membership.anchors())
            + state.warm_memberships.growth_for(key);
        let covered = reservation
            .as_ref()
            .map_or(0, |reservation| additional.min(reservation.bytes));
        self.admit_memory(&mut state, context, additional - covered)?;
        if let Some(reservation) = reservation {
            state.transient_bytes -= covered;
            reservation.bytes -= covered;
        }
        state.insert_warm_membership(key.clone(), membership.clone());
        Ok(())
    }

    fn warm_registered(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut remaining = limits(request)?;
        if context["work_class"].as_str() != Some("background") {
            return Err(invalid("registered warming requires background work class"));
        }
        if request["classifier"] != json!({"id":"native","version":"1"}) {
            return Err(invalid("registered warming requires the native classifier"));
        }
        request["thread_ids"]
            .as_array()
            .ok_or_else(|| invalid("missing registered thread ids"))?;
        let roots = request["roots"]
            .as_array()
            .ok_or_else(|| invalid("missing registered roots"))?;
        request["direct_paths"]
            .as_array()
            .ok_or_else(|| invalid("missing registered direct paths"))?;
        if roots.len() > 64 {
            return Err(invalid("registered warming input exceeds its bound"));
        }
        let start = number(request, "start_index")?;
        let key = Self::warm_membership_key(request, context)?;
        let (membership, mut reservation) = self.registered_membership(
            &key,
            request,
            context,
            cancel,
            usage,
            &mut remaining,
            start == 0,
        )?;
        self.publish_warm_membership(&key, &membership, context, reservation.as_mut())?;
        drop(reservation);
        let members = &membership.members;
        let membership_revision = &membership.revision;
        if start > members.len() {
            return Err(invalid("registered warming index exceeds membership"));
        }
        if start > 0
            && request["membership_revision"].as_str() != Some(membership_revision.as_str())
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "registered warming membership changed",
            ));
        }
        let since = active_since_ns(request)?;
        let read_budget = remaining.max_source_read_bytes;
        let mut next = start;
        let mut steps = 0usize;
        let mut uncached_steps = 0usize;
        let mut dirs = ResolvedDirs::new(self, context);
        'warming: while next < members.len() && steps < 256 && uncached_steps < 8 {
            if let Err(error) = cancel.check(remaining.deadline_unix_ms) {
                if error.status == Status::Deadline && next > start {
                    break 'warming;
                }
                return Err(error);
            }
            let source = &members[next];
            if !active(source, since) {
                next += 1;
                continue;
            }
            let before_read = usage[1];
            let before_events = usage[3];
            let (canonical, metadata) = dirs.resolve(&source.path)?;
            let cache_reads =
                PreparedCacheReads::Warming(remaining.max_source_read_bytes, &membership, since);
            let (stamp, mut outcome) = match self.prepared_source(
                &canonical,
                &metadata,
                context,
                &mut remaining,
                cache_reads,
                cancel,
                usage,
                FactsNeed::Index,
            ) {
                Ok(outcome) => outcome,
                Err(error)
                    if error.status == Status::Incomplete
                        && error.reason == "prepared_cache_read_limit" =>
                {
                    if remaining.max_source_read_bytes < read_budget {
                        break 'warming;
                    }
                    return Err(prepared_cache_entry_limit());
                }
                Err(error)
                    if error.status == Status::Incomplete && error.reason == "source_read_limit" =>
                {
                    break 'warming;
                }
                Err(error)
                    if error.status == Status::Deadline
                        && (next > start || usage[1] > before_read || usage[3] > before_events) =>
                {
                    break 'warming;
                }
                Err(error) => return Err(error),
            };
            if stamp != source.stamp {
                if let PreparedSourceOutcome::Pending(token) = &outcome {
                    self.lock_state()
                        .waiters
                        .remove(token);
                }
                return Err(SnapshotError::new(
                    Status::Changed,
                    "registered source changed",
                ));
            }
            let mut rounds = 0;
            let cached = loop {
                match outcome {
                    PreparedSourceOutcome::Ready { stamp, cached, .. } => {
                        if stamp != source.stamp {
                            return Err(SnapshotError::new(
                                Status::Changed,
                                "registered source changed",
                            ));
                        }
                        next += 1;
                        break cached;
                    }
                    PreparedSourceOutcome::Stored => {
                        next += 1;
                        break true;
                    }
                    PreparedSourceOutcome::Superseded => {
                        unreachable!("index warming never upgrades a listed entry")
                    }
                    PreparedSourceOutcome::Pending(token) => {
                        if rounds == 16 {
                            self.lock_state()
                                .waiters
                                .remove(&token);
                            break 'warming;
                        }
                        #[cfg(test)]
                        self.warm_copies.fetch_add(1, Ordering::Relaxed);
                        let pending = PendingPreparedSource {
                            token: token.clone(),
                            path: source.path.clone(),
                            stamp: source.stamp,
                        };
                        outcome = match self.resume_prepared_source(
                            &pending,
                            &mut dirs,
                            context,
                            &mut remaining,
                            cancel,
                            usage,
                            FactsNeed::Index,
                        ) {
                            Ok(outcome) => outcome,
                            Err(error)
                                if error.status == Status::Incomplete
                                    && error.reason == "source_read_limit"
                                    || error.status == Status::SourceLimit
                                        && error.reason == "cumulative preparation work budget exhausted"
                                    || error.status == Status::Deadline
                                        && (next > start
                                            || usage[1] > before_read
                                            || usage[3] > before_events) =>
                            {
                                self.lock_state()
                                    .waiters
                                    .remove(&token);
                                break 'warming;
                            }
                            Err(error) => {
                                self.lock_state()
                                    .waiters
                                    .remove(&token);
                                return Err(error);
                            }
                        };
                        rounds += 1;
                    }
                }
            };
            steps += 1;
            uncached_steps += usize::from(!cached);
        }
        if !dirs.hold(|| match cancel.check(remaining.deadline_unix_ms) {
            Err(error) if error.status == Status::Deadline => Ok(()),
            checked => checked,
        })? {
            return Err(SnapshotError::new(
                Status::Changed,
                "registered source changed",
            ));
        }
        drop(dirs);
        let complete = next == members.len();
        if complete {
            if !self.validate_warm_membership(
                &membership.members,
                &membership.sidechain_dirs,
                context,
                cancel,
                remaining.deadline_unix_ms,
            )? {
                self.lock_state().remove_warm_membership(&key);
                return Err(SnapshotError::new(
                    Status::Changed,
                    "registered warming membership changed",
                ));
            }
            self.prepared_disk.check_dir()?;
            for source in members.iter().filter(|source| active(source, since)) {
                cancel.check(remaining.deadline_unix_ms)?;
                let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
                    source.stamp,
                    str_field(context, "registry_generation")?,
                    str_field(context, "admission")?,
                    &context["authority"],
                    &request["classifier"],
                )?;
                if !self.prepared_disk.lists(&key) {
                    return Err(SnapshotError::new(
                        Status::RetainedLimit,
                        "prepared facts cache cannot retain complete membership",
                    ));
                }
            }
            self.prepared_disk.check_dir()?;
        }
        let disk = self.prepared_disk.stats();
        let (source_offset, source_size) = if let Some(source) = members.get(next) {
            let load = self.lock_state()
                .prepared_loads
                .get(&source.stamp.identity)
                .map(|(slot, _)| Arc::clone(slot));
            (
                load.map_or(0, |slot| slot.work.lock().expect("source load").offset),
                source.stamp.size,
            )
        } else {
            (0, 0)
        };
        Ok((
            json!({"kind":"warmed_registry","owner_epoch":self.owner_epoch,"membership_revision":membership_revision,"next_index":next,"complete":complete,"source_offset":source_offset,"source_size":source_size,"fact_cache_bytes":disk.bytes,"fact_cache_write_bytes":disk.write_bytes,"fact_cache_writes":disk.writes}),
            None,
            None,
        ))
    }

    fn validate_prepared_root(
        &self,
        graph: &Arc<Mutex<PreparedGraph>>,
        context: &Value,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<(), SnapshotError> {
        let (handle, classifier, stamp, deadline) = {
            let graph = graph.lock().expect("prepared graph");
            (
                graph.root_handle.clone(),
                graph.classifier.clone(),
                graph.root.stamp,
                graph.remaining.deadline_unix_ms,
            )
        };
        let scope = self.renew_scope_for_work(&handle, context, deadline)?;
        let root = scope.snapshot;
        let current = SourceStamp::of(&std::fs::metadata(&root.canonical_path).map_err(|_| {
            SnapshotError::new(Status::Changed, "prepared graph root disappeared")
        })?)
        .viewed_as(stamp);
        if root.stamp != stamp
            || current != stamp && !self.extends_prefix(&root, remaining, usage)?
            || !classifier_eq(&classifier, &scope.classifier)?
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared root generation changed",
            ));
        }
        Ok(())
    }

    fn prepared_query_page(
        &self,
        token: &str,
        mut cursor: PreparedQueryCursor,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut dirs = ResolvedDirs::new(self, context);
        let page = self
            .prepared_query_steps(&mut cursor, &mut dirs, context, reservation, cancel, usage)
            .and_then(|page| {
                if dirs.hold(|| cancel.check(cursor.remaining.deadline_unix_ms))? {
                    Ok(page)
                } else {
                    Err(SnapshotError::new(
                        Status::Changed,
                        "prepared graph source changed",
                    ))
                }
            });
        let pending = cursor.pending.as_ref().map(|pending| pending.token.clone());
        let reply = page.and_then(|page| match page {
            QueryPage::Complete(data) => Ok((data, None, None)),
            QueryPage::Incomplete(data) => {
                self.park_prepared_query(token, cursor, data, context, reservation, cancel)
            }
        });
        if reply.is_err() {
            if let Some(pending) = pending {
                self.lock_state().waiters.remove(&pending);
            }
        }
        reply
    }

    fn prepared_query_steps(
        &self,
        cursor: &mut PreparedQueryCursor,
        dirs: &mut ResolvedDirs<'_>,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<QueryPage, SnapshotError> {
        if cursor.claimant != str_field(context, "claimant")? {
            return Err(SnapshotError::new(
                Status::StaleCursor,
                "prepared query claimant differs",
            ));
        }
        cancel.check(cursor.remaining.deadline_unix_ms)?;
        let graph = self.lock_state()
            .prepared_graphs
            .get(&cursor.graph_id)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "prepared graph expired"))?;
        self.validate_prepared_root(&graph, context, &mut cursor.remaining, usage)?;
        let (sources, sidechain_dirs) = {
            let graph = graph.lock().expect("prepared graph");
            if graph.admission != str_field(context, "admission")?
                || graph.authority != context["authority"]
                || graph.registry_generation != str_field(context, "registry_generation")?
            {
                return Err(SnapshotError::new(
                    Status::StaleHandle,
                    "prepared graph context differs",
                ));
            }
            (Arc::clone(&graph.sources), Arc::clone(&graph.sidechain_dirs))
        };
        let inputs = cursor.input_records.is_some();
        let total = sources.len();
        let mut records = Vec::new();
        let mut bytes = 128usize;
        let mut steps = 0usize;
        let mut uncached_steps = 0usize;
        let page_steps = if inputs { 8 } else { 256 };
        let page_items = self.config.page_items.min(cursor.remaining.max_items);
        let output_limit = cursor
            .page_output_bytes
            .min(cursor.remaining.max_output_bytes)
            .min(MAX_DATA_BYTES);
        if output_limit < 128 || inputs && page_items == 0 {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "prepared query output budget exhausted",
            ));
        }
        while (cursor.next < total
            || cursor.input_records.as_ref().is_some_and(|records| !records.is_empty()))
            && steps < page_steps
            && uncached_steps < 8
            && (!inputs || records.len() < page_items)
        {
            cancel.check(cursor.remaining.deadline_unix_ms)?;
            if let Some(record) = cursor.input_records.as_mut().and_then(VecDeque::pop_front) {
                let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)? + 1;
                if bytes + record_bytes > output_limit {
                    if records.is_empty() {
                        return Err(SnapshotError::new(
                            Status::OutputLimit,
                            "prepared input record exceeds page bound",
                        ));
                    }
                    cursor.input_records.as_mut().expect("input records").push_front(record);
                    break;
                }
                bytes += record_bytes;
                records.push(record);
                continue;
            }
            let source = &sources[cursor.next];
            let (canonical, metadata) = dirs.resolve(&source.path)?;
            if SourceStamp::of(&metadata) != source.stamp {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "prepared graph source changed",
                ));
            }
            let outcome = if let Some(mut pending) = cursor.pending.take() {
                if pending.path != source.path || pending.stamp != source.stamp {
                    return Err(invalid("prepared query source cursor differs"));
                }
                match self.authority(context, Some(&canonical)).and_then(|()| {
                    self.advance_prepared_source(
                        &pending,
                        context,
                        &mut cursor.remaining,
                        cancel,
                        usage,
                        cursor.need,
                    )
                }) {
                    Ok(PreparedSourceOutcome::Pending(next)) => {
                        pending.token = next;
                        cursor.pending = Some(pending);
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
                    Ok(PreparedSourceOutcome::Superseded) => continue,
                    Ok(ready) => ready,
                    Err(error) => {
                        self.lock_state()
                            .waiters
                            .remove(&pending.token);
                        return Err(error);
                    }
                }
            } else {
                match self.prepared_source(
                    &canonical,
                    &metadata,
                    context,
                    &mut cursor.remaining,
                    PreparedCacheReads::Unmetered,
                    cancel,
                    usage,
                    cursor.need,
                )? {
                    (_, PreparedSourceOutcome::Pending(source_cursor)) => {
                        #[cfg(test)]
                        self.warm_copies.fetch_add(1, Ordering::Relaxed);
                        cursor.pending = Some(PendingPreparedSource {
                            token: source_cursor,
                            path: source.path.clone(),
                            stamp: source.stamp,
                        });
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
                    (_, ready) => ready,
                }
            };
            let PreparedSourceOutcome::Ready {
                stamp,
                ref held,
                cached,
            } = outcome
            else {
                return Err(invalid("prepared source did not finish"));
            };
            self.after_facts_returned();
            if stamp != source.stamp {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "prepared source revision changed",
                ));
            }
            let facts = held.facts();
            if inputs {
                let bound = crate::snapshot_codec::predicate_input_records_bound(&facts.inputs)?;
                let queue = cursor.input_records.as_mut().expect("input records");
                self.extend_projection_reservation(
                    reservation,
                    context,
                    deque_growth(queue, bound.records) + bound.bytes,
                )?;
                let records = crate::snapshot_codec::predicate_input_records(&facts.inputs, &bound)?;
                let predicted = deque_capacity_for(queue, records.len());
                queue.extend(records);
                assert_eq!(queue.capacity(), predicted);
            } else {
                let found = facts.query(&cursor.query)?["value"].as_bool() == Some(true);
                cursor.observers.observe(facts);
                if found {
                    return Ok(QueryPage::Complete(cursor.observers.reply(
                        true,
                        false,
                        output_limit,
                        self.config.output,
                    )?));
                }
            }
            cursor.next += 1;
            steps += 1;
            uncached_steps += usize::from(!cached);
        }
        if cursor.next == total
            && cursor.input_records.as_ref().is_none_or(VecDeque::is_empty)
        {
            if !self.validate_source_stamps(
                &sources,
                context,
                cancel,
                cursor.remaining.deadline_unix_ms,
            )? || !self.validate_sidechain_dirs(
                &sidechain_dirs,
                context,
                cancel,
                cursor.remaining.deadline_unix_ms,
            )?
            {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "prepared graph sidechain membership changed",
                ));
            }
            let data = if inputs {
                json!({"kind":"records","record_schema":"cc-transcript.predicate-inputs/1","records_json":records})
            } else {
                cursor.observers.reply(false, true, output_limit, self.config.output)?
            };
            encoded_size(&data, output_limit)?;
            return Ok(QueryPage::Complete(data));
        }
        let data = if inputs {
            json!({"kind":"records","record_schema":"cc-transcript.predicate-inputs/1","records_json":records})
        } else {
            Value::new_null()
        };
        cancel.check(cursor.remaining.deadline_unix_ms)?;
        let encoded = encoded_size(&data, output_limit)?;
        cursor.remaining.max_output_bytes = cursor.remaining.max_output_bytes.saturating_sub(encoded);
        cursor.remaining.max_items = cursor.remaining.max_items.saturating_sub(
            data.get("records_json")
                .and_then(Value::as_array)
                .map_or(0, |items| items.len()),
        );
        Ok(QueryPage::Incomplete(data))
    }

    fn park_prepared_query(
        &self,
        token: &str,
        cursor: PreparedQueryCursor,
        data: Value,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut state = self.lock_state();
        cancel.check(cursor.remaining.deadline_unix_ms)?;
        if !state.prepared_graphs.contains_key(&cursor.graph_id) {
            return Err(SnapshotError::new(Status::StaleCursor, "prepared graph released"));
        }
        if state.prepared_queries.len() >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared query admission exhausted",
            ));
        }
        let token = token.to_owned();
        let pledge = Delivery::cursor_pledge(&cursor.claimant, &token);
        let additional =
            state.admission(&token, &cursor, []) + state.prepared_queries.growth_for(&token) + pledge;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.prepared_queries.reserve_for(&token);
        state.prepared_queries.insert(token.clone(), cursor);
        state.prepared_queries.pledge(&token, pledge);
        Ok((data, Some(token), Some("prepared query page incomplete".to_owned())))
    }

    fn query_graph(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let handle = request
            .get("handle")
            .ok_or_else(|| invalid("missing prepared graph handle"))?;
        if str_field(handle, "owner_epoch")? != self.owner_epoch {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph owner epoch differs",
            ));
        }
        let token = str_field(handle, "graph_id")?;
        let shared = self
            .lock_state()
            .prepared_graphs
            .get(token)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "prepared graph expired"))?;
        let mut bounds = limits(request)?;
        self.validate_prepared_root(&shared, context, &mut bounds, usage)?;
        let mut graph = shared.lock().expect("prepared graph");
        if graph.claimant != str_field(context, "claimant")?
            || graph.registry_generation != str_field(context, "registry_generation")?
            || graph.admission != str_field(context, "admission")?
            || graph.authority != context["authority"]
        {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph context differs",
            ));
        }
        if graph.revision != str_field(handle, "revision")?
            || handle["complete"].as_bool() != Some(true)
        {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph revision differs",
            ));
        }
        if !graph.validated {
            for (path, stamp) in graph
                .stamps
                .iter()
                .filter(|(_, stamp)| stamp.identity.file() != graph.root.stamp.identity.file())
            {
                self.authority(context, Some(path))?;
                let current = std::fs::metadata(path).map_err(|_| {
                    SnapshotError::new(Status::Changed, "prepared graph source disappeared")
                })?;
                if SourceStamp::of(&current) != *stamp {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "prepared graph source changed",
                    ));
                }
            }
            graph.validated = true;
        }
        if graph.expires <= now_ms() {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph expired",
            ));
        }
        let query = request
            .get("query")
            .ok_or_else(|| invalid("missing prepared graph query"))?;
        let kind = str_field(query, "kind")?;
        if !matches!(
            kind,
            "deep_predicate_inputs"
                | "has_tool"
                | "has_command"
                | "has_edit_to"
                | "has_read"
                | "has_skill"
                | "has_override"
                | "has_read_glob"
                | "has_skill_suffix"
                | "has_command_regex"
                | "has_error"
                | "has_edit"
        ) {
            return Err(invalid("unsupported prepared graph query"));
        }
        cancel.check(bounds.deadline_unix_ms)?;
        let need = FactsNeed::of_query(query, request.get("observers").and_then(Value::as_array));
        let selectors = request
            .get("selectors")
            .ok_or_else(|| invalid("missing graph selectors"))?;
        let expires = graph.expires;
        let root = Arc::clone(&graph.root);
        let classifier = graph.classifier.clone();
        let selector_key = (!selectors.as_array().is_some_and(|items| items.is_empty()))
            .then(|| sonic_rs::to_string(selectors).map_err(|error| invalid(error.to_string())))
            .transpose()?;
        drop(graph);
        let cached_slice = {
            let mut state = self.lock_state();
            if !state.prepared_graphs.contains_key(token) {
                return Err(SnapshotError::new(
                    Status::StaleHandle,
                    "prepared graph expired",
                ));
            }
            let graph = shared.lock().expect("prepared graph");
            let facts = match &selector_key {
                None => Some(&graph.root_facts),
                Some(key) => {
                    if graph.root_slices.len() >= 64 && !graph.root_slices.contains_key(key) {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "prepared root selector cache exhausted",
                        ));
                    }
                    graph.root_slices.get(key)
                }
            }
            .filter(|facts| facts.serves(need))
            .cloned();
            facts.map(|facts| self.retain_cached_facts(&mut state, facts))
        };
        let retained_root = match (cached_slice, selector_key) {
            (Some(facts), _) => facts,
            (None, Some(key)) => {
                let mut fact_limits = bounds;
                fact_limits.max_read_bytes = self.config.source;
                fact_limits.max_events = root.event_count;
                let bound = crate::snapshot_projection::facts_bound(&root, need);
                let mut slice_reservation = self.reserve_projection(context, bound)?;
                #[cfg(test)]
                self.fact_builds.fetch_add(1, Ordering::Relaxed);
                let (facts, _, _) = crate::snapshot_projection::prepare_facts(
                    &root,
                    selectors,
                    &fact_limits,
                    cancel,
                    need,
                )?;
                assert!(
                    facts.accounted_bytes() <= bound,
                    "prepared root slice outgrew its root bound"
                );
                self.publish_root_slice(
                    token,
                    &shared,
                    key,
                    Arc::new(facts),
                    need,
                    context,
                    &mut slice_reservation,
                )?
            }
            (None, None) => {
                let (retained, reservation) = self.prepared_root_facts(
                    &root,
                    &classifier,
                    context,
                    &bounds,
                    PreparedCacheReads::Unmetered,
                    cancel,
                    usage,
                    need,
                )?;
                drop(reservation);
                retained
            }
        };
        let root_facts = retained_root.facts();
        if kind != "deep_predicate_inputs"
            && root_facts.query(query)?["value"].as_bool() == Some(true)
        {
            let data = json!({"kind":"scalar","value":true});
            encoded_size(&data, bounds.max_output_bytes)?;
            return Ok((data, None, None));
        }
        let claimant = str_field(context, "claimant")?;
        let mut reservation = self.reserve_projection(
            context,
            size_of::<PreparedQueryCursor>() + claimant.len() + token.len() + value_bytes(query),
        )?;
        #[cfg(test)]
        self.prepared_query_records.fetch_add(1, Ordering::Relaxed);
        let input_records = if kind == "deep_predicate_inputs" {
            let bound = crate::snapshot_codec::predicate_input_records_bound(&root_facts.inputs)?;
            self.extend_projection_reservation(&mut reservation, context, bound.bytes)?;
            let records = VecDeque::from(crate::snapshot_codec::predicate_input_records(
                &root_facts.inputs,
                &bound,
            )?);
            assert_eq!(records.capacity(), bound.records);
            Some(records)
        } else {
            None
        };
        let observers = if kind == "deep_predicate_inputs" {
            Observers::default()
        } else {
            self.admit_observers(
                request,
                &root_facts,
                bounds.deadline_unix_ms,
                context,
                &mut reservation,
            )
        };
        let cursor = PreparedQueryCursor {
            claimant: claimant.to_owned(),
            graph_id: token.to_owned(),
            query: query.clone(),
            pending: None,
            input_records,
            observers,
            need,
            next: 0,
            page_output_bytes: bounds.max_output_bytes.min(MAX_DATA_BYTES),
            remaining: bounds,
            expires,
        };
        self.prepared_query_page(
            &self.token("prepared-query"),
            cursor,
            context,
            &mut reservation,
            cancel,
            usage,
        )
    }

    fn admit_observers(
        &self,
        request: &Value,
        root_facts: &crate::snapshot_prepared::PreparedFacts,
        deadline_unix_ms: u64,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Observers {
        let Some(queries) = request
            .get("observers")
            .and_then(Value::as_array)
            .filter(|queries| (1..=OBSERVER_LIMIT).contains(&queries.len()))
        else {
            return Observers::default();
        };
        let started = std::time::Instant::now();
        let mut observers = Observers {
            declared: queries.len(),
            allowance: std::time::Duration::from_millis(
                deadline_unix_ms.saturating_sub(now_ms()) / 4,
            ),
            witnessed: 0,
            pending: Vec::new(),
        };
        let mut pending = 0u64;
        for (index, query) in queries.iter().enumerate() {
            if !Observers::observable(query) {
                continue;
            }
            match Observers::timely(root_facts, query, started, observers.allowance) {
                Some(Some(true)) => observers.witnessed |= 1 << index,
                Some(Some(false)) => pending |= 1 << index,
                Some(None) => {}
                None => break,
            }
        }
        observers.allowance = observers.allowance.saturating_sub(started.elapsed());
        if observers.allowance.is_zero() {
            pending = 0;
        }
        let selected = || {
            queries
                .iter()
                .enumerate()
                .filter(move |(index, _)| pending & (1 << *index) != 0)
        };
        let count = pending.count_ones() as usize;
        let bytes = count * size_of::<(usize, Value)>()
            + selected().map(|(_, query)| value_bytes(query)).sum::<usize>();
        if count > 0 && self.reserve_observers(reservation, context, bytes) {
            observers.pending = Vec::with_capacity(count);
            observers
                .pending
                .extend(selected().map(|(index, query)| (index, query.clone())));
        }
        observers
    }

    fn reserve_observers(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        bytes: usize,
    ) -> bool {
        let fits = self.admit_spare(context, bytes);
        if fits {
            reservation.bytes += bytes;
        }
        fits
    }

    fn admit_spare(&self, context: &Value, bytes: usize) -> bool {
        let mut state = self.lock_state();
        let fits = self.memory_cap(context).is_ok_and(|cap| {
            Self::retained_total(&mut state).saturating_add(bytes) <= cap
        });
        if fits {
            state.ledger.shared.work().admitted(bytes);
            state.transient_bytes += bytes;
            state.ledger.shared.work().reserved(bytes);
        }
        fits
    }

    fn publish_root_slice(
        &self,
        token: &str,
        shared: &Arc<Mutex<PreparedGraph>>,
        key: String,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        need: FactsNeed,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<RetainedFacts<'_>, SnapshotError> {
        assert!(facts.serves(need));
        let mut state = self.lock_state();
        if !state.prepared_graphs.contains_key(token) {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph expired",
            ));
        }
        let mut graph = shared.lock().expect("prepared graph");
        if let Some(existing) = graph
            .root_slices
            .get(&key)
            .filter(|facts| facts.serves(need))
        {
            return Ok(self.retain_cached_facts(&mut state, Arc::clone(existing)));
        }
        let occupied = graph.root_slices.contains_key(&key);
        if graph.root_slices.len() >= 64 && !occupied {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "prepared root selector cache exhausted",
            ));
        }
        let retained = state.ledger.shared.admission([facts_anchor(&facts)])
            + if occupied { 0 } else { key.capacity() }
            + graph.root_slices.growth_for(&key);
        let covered = retained.min(reservation.bytes);
        self.admit_memory(&mut state, context, retained - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_root_slice(&mut graph, key, Arc::clone(&facts));
        drop(graph);
        drop(state.prepared_graphs.get_mut(token));
        Ok(self.retain_cached_facts(&mut state, facts))
    }
}

const OBSERVER_LIMIT: usize = 32;

#[cfg(test)]
thread_local! {
    static OBSERVATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
struct Observers {
    declared: usize,
    allowance: std::time::Duration,
    witnessed: u64,
    pending: Vec<(usize, Value)>,
}

impl Observers {
    fn observable(query: &Value) -> bool {
        matches!(
            query.get("kind").and_then(Value::as_str),
            Some(
                "has_tool"
                    | "has_read"
                    | "has_command"
                    | "has_skill"
                    | "has_skill_suffix"
                    | "has_override"
                    | "has_error"
                    | "has_edit"
            )
        )
    }

    fn observed(facts: &crate::snapshot_prepared::PreparedFacts, query: &Value) -> Option<bool> {
        #[cfg(test)]
        OBSERVATIONS.with(|count| count.set(count.get() + 1));
        facts
            .query(query)
            .ok()
            .map(|answer| answer["value"].as_bool() == Some(true))
    }

    fn timely(
        facts: &crate::snapshot_prepared::PreparedFacts,
        query: &Value,
        started: std::time::Instant,
        allowance: std::time::Duration,
    ) -> Option<Option<bool>> {
        (started.elapsed() < allowance)
            .then(|| Self::observed(facts, query))
            .filter(|_| started.elapsed() < allowance)
    }

    fn charge(&self) -> usize {
        self.pending.capacity() * size_of::<(usize, Value)>()
            + self
                .pending
                .iter()
                .map(|(_, query)| value_bytes(query))
                .sum::<usize>()
    }

    fn observe(&mut self, facts: &crate::snapshot_prepared::PreparedFacts) {
        let Self {
            allowance,
            witnessed,
            pending,
            ..
        } = self;
        if pending.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let mut expired = false;
        pending.retain(|(index, query)| {
            let outcome = (!expired)
                .then(|| Self::timely(facts, query, started, *allowance))
                .flatten();
            expired = outcome.is_none();
            match outcome {
                Some(Some(true)) => {
                    *witnessed |= 1 << *index;
                    false
                }
                Some(Some(false)) => true,
                Some(None) | None => false,
            }
        });
        *allowance = allowance.saturating_sub(started.elapsed());
        if allowance.is_zero() {
            pending.clear();
        }
    }

    fn reply(
        &self,
        value: bool,
        exhausted: bool,
        output_limit: usize,
        projection_limit: usize,
    ) -> Result<Value, SnapshotError> {
        let plain = json!({"kind":"scalar","value":value});
        encoded_size(&plain, output_limit)?;
        if self.declared == 0 {
            return Ok(plain);
        }
        let outcomes: Vec<Option<bool>> = (0..self.declared)
            .map(|index| {
                if self.witnessed & (1 << index) != 0 {
                    Some(true)
                } else if exhausted && self.pending.iter().any(|(pending, _)| *pending == index) {
                    Some(false)
                } else {
                    None
                }
            })
            .collect();
        let observed = json!({"kind":"observed_scalar","value":value,"observers":outcomes});
        Ok(
            if encoded_size(&observed, output_limit.min(projection_limit)).is_ok() {
                observed
            } else {
                plain
            },
        )
    }
}

struct ResolvedDirs<'a> {
    store: &'a NativeStore,
    context: &'a Value,
    dirs: HashMap<PathBuf, PathBuf>,
    reservation: Option<ProjectionReservation<'a>>,
}

impl<'a> ResolvedDirs<'a> {
    fn new(store: &'a NativeStore, context: &'a Value) -> Self {
        Self {
            store,
            context,
            dirs: HashMap::new(),
            reservation: None,
        }
    }

    fn resolve(&mut self, path: &Path) -> Result<(PathBuf, std::fs::Metadata), SnapshotError> {
        let resolved = match self.context["authority"]["kind"].as_str() {
            Some("user") => match std::fs::symlink_metadata(path).map_err(io_error)? {
                metadata if metadata.is_symlink() => self.exact(path)?,
                metadata => (self.memoized(path)?, metadata),
            },
            _ => self.exact(path)?,
        };
        self.store.after_source_resolved(path);
        Ok(resolved)
    }

    fn exact(&self, path: &Path) -> Result<(PathBuf, std::fs::Metadata), SnapshotError> {
        let canonical = self.store.canonical_path(path).map_err(io_error)?;
        let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
        Ok((canonical, metadata))
    }

    fn memoized(&mut self, path: &Path) -> Result<PathBuf, SnapshotError> {
        let (parent, name) = path
            .parent()
            .zip(path.file_name())
            .expect("resolved sources are file paths");
        if let Some(directory) = self.dirs.get(parent) {
            return Ok(directory.join(name));
        }
        let directory = self.store.canonical_path(parent).map_err(io_error)?;
        let canonical = directory.join(name);
        let key = parent.to_path_buf();
        let growth = map_growth(&self.dirs, 1) + key.capacity() + directory.capacity();
        if self.store.admit_spare(self.context, growth) {
            self.reservation
                .get_or_insert_with(|| ProjectionReservation {
                    store: self.store,
                    bytes: 0,
                })
                .bytes += growth;
            let predicted = map_capacity_for(&self.dirs, 1);
            self.dirs.insert(key, directory);
            assert_eq!(self.dirs.capacity(), predicted);
        }
        Ok(canonical)
    }

    fn hold(&self, check: impl Fn() -> Result<(), SnapshotError>) -> Result<bool, SnapshotError> {
        check()?;
        for (directory, canonical) in &self.dirs {
            check()?;
            match self.store.canonical_path(directory) {
                Ok(current) if current == *canonical => {}
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error(error)),
            }
        }
        check()?;
        Ok(true)
    }
}

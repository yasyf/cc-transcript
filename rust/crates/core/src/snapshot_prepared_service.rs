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
            evidence,
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
        let evidence_bytes = match evidence {
            crate::snapshot_prepared::PreparedEvidence::Index { inputs, .. } => Self::audit_value_bytes(inputs),
            crate::snapshot_prepared::PreparedEvidence::Skills {
                evidence: crate::snapshot_prepared::SkillEvidence::Parsed { skills },
            } => skills.capacity() * size_of::<String>() + skills.iter().map(String::capacity).sum::<usize>(),
            crate::snapshot_prepared::PreparedEvidence::Skills {
                evidence: crate::snapshot_prepared::SkillEvidence::CodexFramed { .. },
            } => size_of::<crate::snapshot_prepared::CodexSkillProof>(),
        };
        arc_mirror::<crate::snapshot_prepared::PreparedFacts>() + evidence_bytes + override_bytes
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
        drop(add);
        let mut add_append = |append: &Arc<CompactCodexIndex>| {
            if seen.insert(Arc::as_ptr(append) as usize) {
                bytes += Self::audit_compact_codex_bytes(append);
            }
        };
        for cached in state.prepared_facts.values() {
            if let Some(append) = &cached.append { add_append(append); }
        }
        for owner in &state.retained_append_owners {
            add_append(&owner.upgrade().expect("retained compact provenance is live"));
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
        let memberships = state
            .warm_memberships
            .values()
            .filter_map(|slot| slot.metadata())
            .chain(
                state
                    .retained_membership_owners
                    .iter()
                    .map(|owner| owner.upgrade().expect("retained membership is live")),
            );
        for membership in memberships {
            add(
                Arc::as_ptr(&membership) as usize,
                arc_mirror::<WarmMembership>() + membership.revision.capacity(),
            );
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
                Arc::as_ptr(&graph.header) as usize,
                arc_mirror::<PreparedGraphHeader>()
                    + value_bytes(&graph.header.handle)
                    + value_bytes(&graph.header.classifier),
            );
            add(
                slice_key(&graph.sidechain_dirs),
                dirs(&graph.sidechain_dirs),
            );
        }
        for (owner, field) in &state.retained_graph_input_owners {
            let source = owner.upgrade().expect("retained graph sources are live");
            let header = field.upgrade().expect("retained graph header is live");
            add(slice_key(&source), sources(&source));
            add(
                Arc::as_ptr(&header) as usize,
                arc_mirror::<PreparedGraphHeader>()
                    + value_bytes(&header.handle)
                    + value_bytes(&header.classifier),
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
        mut append: Option<Arc<CompactCodexIndex>>,
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
        let budget = self.prepared_fact_budget();
        if append.as_ref().is_some_and(|append| facts.accounted_bytes().saturating_add(append.accounted_bytes()) > budget) {
            append = None;
        }
        let accounted = facts.accounted_bytes() + append.as_ref().map_or(0, |append| append.accounted_bytes());
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
            append,
            last_used: now_ms(),
        };
        let anchors = std::iter::once(facts_anchor(&cached.facts))
            .chain(cached.append.as_ref().map(append_anchor));
        let additional = state.admission(&key, &cached, anchors)
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
                            None,
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
                    crate::snapshot_prepared_disk::DiskLookup::IndexOnly(compact)
                | crate::snapshot_prepared_disk::DiskLookup::SkillsOnly(compact) => {
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
                None,
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
        let mut skill_admission = if need == FactsNeed::Skills {
            Some(self.admit_skill_source(context)?)
        } else { None };
        let mut skill_file = if need == FactsNeed::Skills {
            Some(self.open_skill_source(canonical, stamp, context, remaining.deadline_unix_ms, cancel, usage)?)
        } else { None };
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
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "request" });
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
            #[cfg(test)]
            self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "retained-hit" });
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
            #[cfg(test)]
            self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "retained-hit" });
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
                    if !facts.validates_source(stamp.size, self.config.entry) {
                        return Err(invalid("prepared skill framing proof differs from source bounds"));
                    }
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-hit" });
                    usage[7] += 1;
                    let facts = Arc::new(facts);
                    let held = match self.cache_prepared_facts(
                        stamp,
                        Arc::clone(&facts),
                        &classifier,
                        context,
                        &mut reservation,
                        need,
                        None,
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
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-retired" });
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "prepared facts revision was evicted",
                    ));
                }
                crate::snapshot_prepared_disk::DiskLookup::Changed => {
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-changed" });
                    continue;
                }
                crate::snapshot_prepared_disk::DiskLookup::IndexOnly(compact) => {
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-index-only" });
                    drop(compact);
                    drop(reservation);
                }
                crate::snapshot_prepared_disk::DiskLookup::SkillsOnly(skills) => {
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-skills-only" });
                    drop(skills);
                    drop(reservation);
                }
                crate::snapshot_prepared_disk::DiskLookup::Miss => {
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-miss" });

                }
            }
        } else {
            #[cfg(test)]
            self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "disk-absent" });
        }
        if need == FactsNeed::Index {
            if let Some(ready) = self.append_compact_codex(canonical, stamp, context, &classifier, remaining, cancel, usage)? {
                return Ok((stamp, ready));
            }
        }
        drop(claim);
        let parsed_claude = need == FactsNeed::Skills && self.lock_state().latest_source(&stamp.identity)
            .is_some_and(|snapshot| snapshot.stamp == stamp && snapshot.provider == Provider::Claude);
        if need == FactsNeed::Skills && !parsed_claude {
            return self.start_skill_source(canonical, stamp, key, skill_file.take().expect("Skills source was opened"), skill_admission.take().expect("Skills source was admitted"), context, remaining, cancel)
                .map(|outcome| (stamp, outcome));
        }
        match self.acquire_prepared_source(canonical, stamp, key, context, remaining, cancel, usage, need)? {
            (_, PreparedSourceOutcome::Superseded) => continue,
            ready => return Ok(ready),
        }
        }
    }

    fn acquire_prepared_source(
        &self,
        canonical: &Path,
        stamp: SourceStamp,
        key: PreparedDiskKey,
        context: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        need: FactsNeed,
    ) -> Result<(SourceStamp, PreparedSourceOutcome<'_>), SnapshotError> {
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Facts { stamp, key, need, disposition: "acquire" });
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
        self.finish_prepared_source(stamp, &outcome.0, context, remaining, cancel, need)
            .map(|result| (stamp, result))
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
        let prepared = prepared.and_then(|(mut reservation, facts)| {
            let append = if need == FactsNeed::Index {
                self.capture_compact_codex(&snapshot, context, cancel, remaining.deadline_unix_ms, &mut reservation)?
            } else { None };
            Ok((reservation, facts, append))
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
                while state.recent_codex_raw_bytes > self.recent_codex_raw_bound() {
                    let oldest = state
                        .recent_codex
                        .iter()
                        .min_by_key(|(_, touched)| *touched)
                        .map(|(identity, _)| *identity)
                        .expect("raw codex cache exceeds bound");
                    #[cfg(test)]
                    let before = (state.recent_codex_raw_bytes, state.latest.contains_key(&oldest), state.source_spines.contains_key(&oldest.file()));
                    state.remove_recent_codex(&oldest);
                    state.remove_latest(&oldest);
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::RawVictim {
                        identity: oldest, before_bytes: before.0, after_bytes: state.recent_codex_raw_bytes,
                        latest_before: before.1, spine_before: before.2,
                        latest_after: state.latest.contains_key(&oldest), spine_after: state.source_spines.contains_key(&oldest.file()),
                    });
                }
            } else if snapshot.provider == Provider::Codex {
                state.remove_recent_codex(&stamp.identity);
                state.remove_latest(&stamp.identity);
            }
        }
        let (reservation, facts, append) = prepared?;
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
        let result = self.publish_prepared_evidence(stamp, reservation, facts, append, context, need);
        if result.is_ok() {
            self.lock_state().prepared_loads.remove(&stamp.identity);
        }
        result
    }

    fn publish_prepared_evidence<'a>(
        &'a self,
        stamp: SourceStamp,
        mut reservation: ProjectionReservation<'a>,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        append: Option<Arc<CompactCodexIndex>>,
        context: &Value,
        need: FactsNeed,
    ) -> Result<PreparedSourceOutcome<'a>, SnapshotError> {
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
            drop(append);
            drop(facts);
            drop(reservation);
            return Ok(PreparedSourceOutcome::Superseded);
        }
        let held = match self.cache_prepared_facts(
            stamp,
            Arc::clone(&facts),
            &classifier,
            context,
            &mut reservation,
            need,
            append,
        ) {
            Ok(retained) => HeldFacts::Retained(retained),
            Err(error) if error.status == Status::RetainedLimit => {
                HeldFacts::Reserved { facts, reservation }
            }
            Err(error) => return Err(error),
        };
        Ok(PreparedSourceOutcome::Ready {
            stamp,
            held,
            cached: false,
        })
    }

    fn membership_collection_key(
        request: &Value,
        context: &Value,
    ) -> Result<String, SnapshotError> {
        let binding = json!({
            "thread_ids": request["thread_ids"],
            "roots": request["roots"],
            "direct_paths": request["direct_paths"],
            "registry_generation": context["registry_generation"],
            "admission": context["admission"],
            "authority": context["authority"],
        });
        let canonical =
            crate::ids::canonical_json(&binding).map_err(|error| invalid(error.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(canonical.as_bytes())))
    }

    fn scoped_warm_revision(key: &str, membership: &WarmMembership) -> String {
        let mut digest = Sha256::new();
        digest.update(b"registered-warm-v1\0");
        digest.update(key.as_bytes());
        digest.update(membership.revision.as_bytes());
        format!("{:x}", digest.finalize())
    }

    fn warm_membership_key(request: &Value, context: &Value) -> Result<String, SnapshotError> {
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
        absences: Option<&SidechainAbsences<'_>>,
    ) -> Result<bool, SnapshotError> {
        Ok(self.validate_source_stamps(members, context, cancel, deadline)?
            && self.validate_sidechain_dirs(sidechain_dirs, context, cancel, deadline, absences)?)
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
        absences: Option<&SidechainAbsences<'_>>,
    ) -> Result<bool, SnapshotError> {
        let mut dirs = ResolvedDirs::new(self, context);
        for (index, (path, stamp)) in sidechain_dirs.iter().enumerate() {
            cancel.check(deadline)?;
            if absences.is_some_and(|proof| proof.contains(index)) { continue; }
            #[cfg(test)]
            self.sidechain_path_probes.fetch_add(1, Ordering::Relaxed);
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
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
        remaining: WorkLimits,
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
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
            let key = Self::membership_collection_key(request, context)?;
            let mut membership_limits = remaining;
            let membership = self.membership_work(
                &key,
                request,
                context,
                cancel,
                usage,
                &mut membership_limits,
                true,
            )?;
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
                + arc_bytes::<PreparedGraphHeader>()
                + value_bytes(root_handle)
                + value_bytes(&view["classifier"])
                + 2 * <Sha256 as Digest>::output_size();
            self.extend_projection_reservation(
                &mut reservation,
                context,
                PreparedGraph::key_charge(&graph_id)
                    + PREPARED_GRAPH_BYTES
                    + record
                    + buffers,
            )?;
            let sources: Arc<[PreparedSourceRef]> = if copied {
                #[cfg(test)]
                self.registered_sources.fetch_add(1, Ordering::Relaxed);
                others().cloned().collect()
            } else {
                Arc::clone(&members)
            };
            let mut digest = Sha256::new();
            digest.update(root.canonical_path.as_os_str().as_encoded_bytes());
            digest.update(root.stamp.revision().as_bytes());
            for source in sources.iter() {
                digest.update(source.path.as_os_str().as_encoded_bytes());
                digest.update(source.stamp.revision().as_bytes());
            }
            let revision = format!("{:x}", digest.finalize());
            let work = self.lock_state().ledger.shared.work().clone();
            let graph = PreparedGraph {
                claimant: str_field(context, "claimant")?.to_owned(),
                registry_generation: str_field(context, "registry_generation")?.to_owned(),
                admission: str_field(context, "admission")?.to_owned(),
                authority: context["authority"].clone(),
                root,
                header: Arc::new(PreparedGraphHeader {
                    handle: root_handle.clone(),
                    classifier: view["classifier"].clone(),
                }),
                root_facts: Arc::clone(retained.facts()),
                root_slices: Table::new(work),
                revision: revision.clone(),
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
                + arc_bytes::<PreparedGraphHeader>()
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
            header: Arc::new(PreparedGraphHeader {
                handle: build.root_handle,
                classifier: build.classifier,
            }),
            root_facts: build.root_facts,
            root_slices: Table::new(work),
            revision: revision.clone(),
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

    fn catalog_membership_valid(
        &self,
        certificate: Option<CatalogCertificate>,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        remaining: &mut WorkLimits,
    ) -> Result<bool, SnapshotError> {
        let Some(certificate) = certificate else { return Ok(true); };
        let ids = request["thread_ids"].as_array().ok_or_else(|| invalid("missing registered thread ids"))?;
        let roots = request["roots"].as_array().ok_or_else(|| invalid("missing registered roots"))?;
        match self.catalog_access(CatalogInput { ids, roots }, context, cancel, remaining,
            &mut [0; 20], CatalogMode::Check(certificate))?
        {
            CatalogResolution::Checked(valid) => Ok(valid),
            _ => unreachable!("catalog certificate validation"),
        }
    }

    fn refreshed_warm_membership(
        &self,
        key: &str,
        cached: &WarmMembership,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        remaining: &mut WorkLimits,
    ) -> Result<Option<(WarmMembership, ProjectionReservation<'_>)>, SnapshotError> {
        if !self.catalog_membership_valid(cached.catalog, request, context, cancel, remaining)?
            || cached.expires <= now_ms()
            || !self.validate_sidechain_dirs(
                &cached.sidechain_dirs,
                context,
                cancel,
                remaining.deadline_unix_ms,
                None,
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
        if !self.catalog_membership_valid(cached.catalog, request, context, cancel, remaining)? {
            return Ok(None);
        }
        self.extend_projection_reservation(
            &mut reservation,
            context,
            arc_slice_bytes::<PreparedSourceRef>(members.len()),
        )?;
        Ok(Some((
            WarmMembership {
                catalog: cached.catalog,
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

    #[cfg(test)]
    fn publish_warm_membership<'a>(
        &'a self,
        key: &String,
        membership: &Arc<WarmMembership>,
        context: &Value,
        reservation: &mut ProjectionReservation<'a>,
    ) -> Result<(), SnapshotError> {
        let slot = self.membership_slot(key, context, reservation)?;
        let prior = {
            let _state = self.lock_state();
            let mut cell = slot.cell.lock().expect("registered membership");
            match std::mem::replace(&mut *cell, MembershipCell::Busy) {
                MembershipCell::Empty => None,
                MembershipCell::Collected(prior) | MembershipCell::Positive(prior) => Some(prior),
                MembershipCell::Pending(_) | MembershipCell::Busy => {
                    panic!("test publication owns no in-flight work")
                }
            }
        };
        self.restore_membership(
            key,
            &slot,
            MembershipCell::Positive(Arc::clone(membership)),
            prior,
            Ok(Arc::clone(membership)),
            context,
            reservation,
        )
        .map(drop)
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
        let collection_key = Self::membership_collection_key(request, context)?;
        let membership = self.membership_work(
            &collection_key,
            request,
            context,
            cancel,
            usage,
            &mut remaining,
            start == 0,
        )?;
        let members = &membership.members;
        let _scope_reservation =
            self.reserve_projection(context, 2 * <Sha256 as Digest>::output_size())?;
        let membership_revision = Self::scoped_warm_revision(&key, &membership);
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
                    PreparedSourceOutcome::SkillPending { .. } => {
                        unreachable!("Index warming cannot create Skills framing")
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
                None,
            )? {
                self.lock_state().remove_warm_membership(&collection_key);
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
                if !self.prepared_disk.lists_for(&key, FactsNeed::Index) {
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
        token: &str,
        context: &Value,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<(), SnapshotError> {
        let validation = self
            .borrow_prepared_graph_validation(token)
            .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "prepared graph expired"))?;
        self.validate_prepared_root_fields(
            &validation.root,
            validation.inputs.header(),
            context,
            remaining,
            usage,
        )
        .map(|_| ())
    }

    fn prepared_root_observation(
        &self,
        root: &PreparedRootValidation,
        header: &PreparedGraphHeader,
        context: &Value,
    ) -> Result<(Arc<TranscriptSnapshot>, SourceStamp), SnapshotError> {
        let scope = self.renew_scope_for_work(&header.handle, context, root.deadline)?;
        let snapshot = scope.snapshot;
        let physical =
            SourceStamp::of(&std::fs::metadata(&snapshot.canonical_path).map_err(|_| {
                SnapshotError::new(Status::Changed, "prepared graph root disappeared")
            })?);
        if snapshot.stamp != root.stamp || !classifier_eq(&header.classifier, &scope.classifier)? {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared root generation changed",
            ));
        }
        Ok((snapshot, physical))
    }

    fn validate_prepared_root_fields(
        &self,
        root: &PreparedRootValidation,
        header: &PreparedGraphHeader,
        context: &Value,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<SourceStamp, SnapshotError> {
        let (snapshot, physical) = self.prepared_root_observation(root, header, context)?;
        if physical.viewed_as(root.stamp) != root.stamp
            && !self.extends_prefix(&snapshot, remaining, usage)?
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared root generation changed",
            ));
        }
        Ok(physical)
    }

    fn borrow_prepared_graph_validation(&self, token: &str) -> Option<PreparedGraphValidation<'_>> {
        let mut state = self.lock_state();
        let shared = state.prepared_graphs.get(token).cloned()?;
        let graph = shared.lock().expect("prepared graph");
        let root = PreparedRootValidation {
            stamp: graph.root.stamp,
            deadline: graph.remaining.deadline_unix_ms,
        };
        let validate_members = !graph.validated;
        let inputs = state.retained_graph_inputs(
            self,
            Arc::clone(&graph.sources),
            Arc::clone(&graph.header),
        );
        Some(PreparedGraphValidation {
            inputs,
            root,
            validate_members,
        })
    }

    fn validate_prepared_graph_identity(
        graph: &PreparedGraph,
        handle: &Value,
        context: &Value,
    ) -> Result<(), SnapshotError> {
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
        Ok(())
    }

    fn checked_prepared_graph(
        &self,
        token: &str,
        handle: &Value,
        context: &Value,
        cancel: &Cancellation,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<Arc<Mutex<PreparedGraph>>, SnapshotError> {
        let mut validation = self
            .borrow_prepared_graph_validation(token)
            .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "prepared graph expired"))?;
        let physical = self.validate_prepared_root_fields(
            &validation.root,
            validation.inputs.header(),
            context,
            remaining,
            usage,
        )?;
        {
            let state = self.lock_state();
            let shared = state
                .prepared_graphs
                .get(token)
                .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "prepared graph expired"))?;
            let graph = shared.lock().expect("prepared graph");
            Self::validate_prepared_graph_identity(&graph, handle, context)?;
            if !Arc::ptr_eq(&graph.sources, validation.inputs.sources())
                || graph.root.stamp != validation.root.stamp
                || !Arc::ptr_eq(&graph.header, validation.inputs.header())
            {
                return Err(SnapshotError::new(
                    Status::StaleHandle,
                    "prepared graph changed during validation",
                ));
            }
            validation.validate_members = !graph.validated;
        }
        if validation.validate_members {
            for source in validation.inputs.sources().iter().filter(|source| {
                source.stamp.identity.file() != validation.root.stamp.identity.file()
            }) {
                cancel.check(remaining.deadline_unix_ms)?;
                self.authority(context, Some(&source.path))?;
                let current = std::fs::metadata(&source.path).map_err(|_| {
                    SnapshotError::new(Status::Changed, "prepared graph source disappeared")
                })?;
                #[cfg(test)]
                {
                    let hook = self
                        .graph_validation_hook
                        .lock()
                        .expect("graph validation hook")
                        .clone();
                    if let Some(hook) = hook {
                        hook(&source.path);
                    }
                }
                if SourceStamp::of(&current) != source.stamp {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "prepared graph source changed",
                    ));
                }
            }
            cancel.check(remaining.deadline_unix_ms)?;
            let (_, current) = self.prepared_root_observation(
                &validation.root,
                validation.inputs.header(),
                context,
            )?;
            if current != physical {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "prepared graph root changed during validation",
                ));
            }
        }
        cancel.check(remaining.deadline_unix_ms)?;
        let state = self.lock_state();
        let shared = state
            .prepared_graphs
            .get(token)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "prepared graph expired"))?;
        let mut graph = shared.lock().expect("prepared graph");
        Self::validate_prepared_graph_identity(&graph, handle, context)?;
        if !Arc::ptr_eq(&graph.sources, validation.inputs.sources())
            || graph.root.stamp != validation.root.stamp
            || !Arc::ptr_eq(&graph.header, validation.inputs.header())
        {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph changed during validation",
            ));
        }
        if graph.expires <= now_ms() {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph expired",
            ));
        }
        cancel.check(remaining.deadline_unix_ms)?;
        if validation.validate_members {
            graph.validated = true;
        }
        drop(graph);
        drop(state);
        drop(validation);
        Ok(shared)
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
        let pending = cursor.pending.as_ref().and_then(|pending| match pending {
            PendingQuerySource::Native(pending) => Some(pending.token.clone()),
            PendingQuerySource::Skills(_) => None,
        });
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
        self.validate_prepared_root(&cursor.graph_id, context, &mut cursor.remaining, usage)?;
        let graph = self.lock_state()
            .prepared_graphs
            .get(&cursor.graph_id)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "prepared graph expired"))?;
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
            let outcome = match cursor.pending.take() {
                Some(PendingQuerySource::Skills(mut work)) => {
                if work.path != canonical || work.stamp != source.stamp {
                    return Err(invalid("skill source cursor differs"));
                }
                match self.advance_skill_source(&mut work, context, &mut cursor.remaining, reservation, cancel, usage)? {
                    SkillStep::Pending => {
                        cursor.pending = Some(PendingQuerySource::Skills(work));
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
                    SkillStep::Native => {
                        let key = work.key;
                        drop(work);
                        match self.acquire_prepared_source(&canonical, source.stamp, key, context, &mut cursor.remaining,
                            cancel, usage, cursor.need)? {
                            (_, PreparedSourceOutcome::Pending(token)) => {
                                cursor.pending = Some(PendingQuerySource::Native(PendingPreparedSource { token, path: source.path.clone(), stamp: source.stamp }));
                                steps += 1;
                                uncached_steps += 1;
                                continue;
                            }
                            (_, PreparedSourceOutcome::Superseded) => continue,
                            (_, ready) => ready,
                        }
                    }
                    SkillStep::Ready(PreparedSourceOutcome::Superseded) => continue,
                    SkillStep::Ready(ready) => ready,
                }
                }
                Some(PendingQuerySource::Native(mut pending)) => {
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
                        cursor.pending = Some(PendingQuerySource::Native(pending));
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
                }
                None => {
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
                        cursor.pending = Some(PendingQuerySource::Native(PendingPreparedSource {
                            token: source_cursor,
                            path: source.path.clone(),
                            stamp: source.stamp,
                        }));
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
                    (_, PreparedSourceOutcome::SkillPending { work, reservation: mut owned }) => {
                        reservation.bytes += owned.bytes;
                        owned.bytes = 0;
                        cursor.pending = Some(PendingQuerySource::Skills(work));
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
                    (_, PreparedSourceOutcome::Superseded) => continue,
                    (_, ready) => ready,
                }
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
                let bound = crate::snapshot_codec::predicate_input_records_bound(facts.index_inputs())?;
                let queue = cursor.input_records.as_mut().expect("input records");
                self.extend_projection_reservation(
                    reservation,
                    context,
                    deque_growth(queue, bound.records) + bound.bytes,
                )?;
                let records = crate::snapshot_codec::predicate_input_records(facts.index_inputs(), &bound)?;
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
                None,
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
        drop(shared);
        let shared = self.checked_prepared_graph(token, handle, context, cancel, &mut bounds, usage)?;
        let graph = shared.lock().expect("prepared graph");
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
        let need = FactsNeed::of_query(query, request.get("observers"));
        let selectors = request
            .get("selectors")
            .ok_or_else(|| invalid("missing graph selectors"))?;
        let expires = graph.expires;
        let root = Arc::clone(&graph.root);
        let classifier = graph.header.classifier.clone();
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
            let bound = crate::snapshot_codec::predicate_input_records_bound(root_facts.index_inputs())?;
            self.extend_projection_reservation(&mut reservation, context, bound.bytes)?;
            let records = VecDeque::from(crate::snapshot_codec::predicate_input_records(
                root_facts.index_inputs(),
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

pub(crate) const OBSERVER_LIMIT: usize = 32;

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

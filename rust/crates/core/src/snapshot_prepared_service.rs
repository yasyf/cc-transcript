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
    fn audit_value_bytes(value: &Value) -> usize {
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
            override_events,
            accounted: _,
        } = facts;
        arc_mirror::<crate::snapshot_prepared::PreparedFacts>()
            + Self::audit_value_bytes(inputs)
            + override_events.as_ref().map_or(0, |events| {
                events.capacity() * size_of::<crate::snapshot_prepared::OverrideEvent>()
                    + events
                        .iter()
                        .map(|crate::snapshot_prepared::OverrideEvent { text, tools }| {
                            text.capacity()
                                + tools.capacity() * size_of::<String>()
                                + tools.iter().map(String::capacity).sum::<usize>()
                        })
                        .sum::<usize>()
            })
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

    fn cache_prepared_facts(
        &self,
        stamp: SourceStamp,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        classifier: &Value,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<RetainedFacts<'_>, SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let authority = &context["authority"];
        let mut state = self.lock_state();
        if let Some(cached) =
            state.touch_prepared_facts(stamp, registry_generation, admission, authority, classifier)
        {
            return Ok(self.retain_cached_facts(&mut state, cached));
        }
        state.remove_prepared_facts(&stamp.identity);
        let accounted = facts.accounted_bytes();
        let budget = self.config.retained.min(self.config.prepared_fact_memory);
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
        let additional = state.admission(&stamp.identity, &cached, [facts_anchor(&cached.facts)])
            + state.prepared_facts_growth(&stamp.identity);
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, context, additional - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_prepared_facts(stamp.identity, cached);
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
        cancel: &Cancellation,
    ) -> Result<(RetainedFacts<'_>, ProjectionReservation<'_>), SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        if let Some(retained) = self.touch_retained_facts(
            root.stamp,
            registry_generation,
            context["admission"].as_str().unwrap_or(""),
            &context["authority"],
            classifier,
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
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            root.stamp,
            registry_generation,
            str_field(context, "admission")?,
            &context["authority"],
            classifier,
        )?;
        let bound = crate::snapshot_projection::facts_bound(root);
        let decoded = self.prepared_disk.decoded_bytes(&key)?;
        let mut reservation = self.reserve_projection(context, decoded.unwrap_or(0).max(bound))?;
        if decoded.is_some() {
            #[cfg(test)]
            self.fact_lookups.fetch_add(1, Ordering::Relaxed);
            match self.prepared_disk.lookup(&key)? {
                crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                    let facts = Arc::new(facts);
                    return match self.cache_prepared_facts(
                        root.stamp,
                        Arc::clone(&facts),
                        classifier,
                        context,
                        &mut reservation,
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
                crate::snapshot_prepared_disk::DiskLookup::Miss => {}
            }
        }
        let mut fact_limits = *remaining;
        fact_limits.max_read_bytes = self.config.source;
        fact_limits.max_events = root.event_count;
        #[cfg(test)]
        self.fact_builds.fetch_add(1, Ordering::Relaxed);
        let (facts, _, _) =
            crate::snapshot_projection::prepare_facts(root, &json!([]), &fact_limits, cancel)?;
        assert!(
            facts.accounted_bytes() <= bound,
            "prepared root facts outgrew their bound"
        );
        self.prepared_disk.insert(
            &key,
            &facts,
            |bytes| {
                self.extend_projection_reservation(&mut reservation, context, bytes)
                    .is_ok()
            },
            |growth| self.admit_prepared_disk_growth(context, growth),
        )?;
        let facts = Arc::new(facts);
        match self.cache_prepared_facts(
            root.stamp,
            Arc::clone(&facts),
            classifier,
            context,
            &mut reservation,
        ) {
            Ok(retained) => Ok((retained, reservation)),
            Err(error) if error.status == Status::RetainedLimit => Ok((
                self.retain_facts(&mut reservation, context, facts)?,
                reservation,
            )),
            Err(error) => Err(error),
        }
    }

    fn prepared_source(
        &self,
        path: &Path,
        context: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(SourceStamp, PreparedSourceOutcome<'_>), SnapshotError> {
        cancel.check(remaining.deadline_unix_ms)?;
        let canonical = std::fs::canonicalize(path).map_err(io_error)?;
        self.authority(context, Some(&canonical))?;
        let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("prepared graph source must be a file"));
        }
        let stamp = SourceStamp::of(&metadata);
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
        if let Some(retained) = self.touch_retained_facts(
            stamp,
            registry_generation,
            context["admission"].as_str().unwrap_or(""),
            &context["authority"],
            &json!({"id":"native","version":"1"}),
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
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            registry_generation,
            str_field(context, "admission")?,
            &context["authority"],
            &json!({"id":"native","version":"1"}),
        )?;
        if let Some(decoded) = self.prepared_disk.decoded_bytes(&key)? {
            let mut reservation = self.reserve_projection(context, decoded)?;
            #[cfg(test)]
            self.fact_lookups.fetch_add(1, Ordering::Relaxed);
            match self.prepared_disk.lookup(&key)? {
                crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                    usage[7] += 1;
                    let facts = Arc::new(facts);
                    let held = match self.cache_prepared_facts(
                        stamp,
                        Arc::clone(&facts),
                        &json!({"id":"native","version":"1"}),
                        context,
                        &mut reservation,
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
                crate::snapshot_prepared_disk::DiskLookup::Miss => {}
            }
        }
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
        let result = if let Some(cursor) = outcome.1 {
            PreparedSourceOutcome::Pending(cursor)
        } else {
            self.finish_prepared_source(stamp, &outcome.0, context, remaining, cancel)?
        };
        Ok((stamp, result))
    }

    fn resume_prepared_source(
        &self,
        pending: &PendingPreparedSource,
        context: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        self.authority(
            context,
            Some(&std::fs::canonicalize(&pending.path).map_err(io_error)?),
        )?;
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
            self.finish_prepared_source(pending.stamp, &outcome.0, context, remaining, cancel)
        }
    }

    fn finish_prepared_source(
        &self,
        stamp: SourceStamp,
        outcome: &Value,
        context: &Value,
        remaining: &WorkLimits,
        cancel: &Cancellation,
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        let handle = &outcome["description"]["handle"];
        let pinned = self
            .pin_scope_for_work(handle, context, remaining.deadline_unix_ms)
            .and_then(|(snapshot, _)| {
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
        let bound = crate::snapshot_projection::facts_bound(&snapshot);
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
        self.prepared_disk.insert(
            &key,
            &facts,
            |bytes| {
                self.extend_projection_reservation(&mut reservation, context, bytes)
                    .is_ok()
            },
            |growth| self.admit_prepared_disk_growth(context, growth),
        )?;
        let held = match self.cache_prepared_facts(
            stamp,
            Arc::clone(&facts),
            &classifier,
            context,
            &mut reservation,
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
        for source in sources {
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
            cancel.check(deadline)?;
            let canonical = match std::fs::canonicalize(&source.path) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(io_error(error)),
            };
            self.authority(context, Some(&canonical))?;
            if canonical != source.path
                || SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?)
                    != source.stamp
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn validate_sidechain_dirs(
        &self,
        sidechain_dirs: &[(PathBuf, Option<SourceStamp>)],
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<bool, SnapshotError> {
        for (path, stamp) in sidechain_dirs {
            #[cfg(test)]
            self.membership_metadata_checks
                .fetch_add(1, Ordering::Relaxed);
            cancel.check(deadline)?;
            let canonical = match std::fs::canonicalize(path) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if stamp.is_some() {
                        return Ok(false);
                    }
                    continue;
                }
                Err(error) => return Err(io_error(error)),
            };
            self.authority(context, Some(&canonical))?;
            let current = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
            if stamp.is_none_or(|stamp| current != stamp) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn prepare_graph(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
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
        let (root, description) =
            self.pin_scope_for_work(root_handle, context, remaining.deadline_unix_ms)?;
        if !classifier_eq(&view["classifier"], &description["classifier"])? {
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
        let direct = request
            .get("direct_paths")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("missing direct_paths"))?;
        if ids.len() > 1024 || roots.len() > 64 || direct.len() > 1024 {
            return Err(invalid("prepared registry input exceeds its bound"));
        }
        if !ids.is_empty() {
            let key = Self::warm_membership_key(request, context)?;
            let (members, sidechain_dirs, complete) = self
                .lock_state()
                .warm_memberships
                .get(&key)
                .map(|membership| {
                    (
                        Arc::clone(&membership.members),
                        Arc::clone(&membership.sidechain_dirs),
                        membership.complete,
                    )
                })
                .ok_or_else(|| {
                    SnapshotError::new(Status::Incomplete, "registered membership is not warmed")
                })?;
            if !complete
                || !self.validate_warm_membership(
                    &members,
                    &sidechain_dirs,
                    context,
                    cancel,
                    remaining.deadline_unix_ms,
                )?
            {
                self.lock_state().remove_warm_membership(&key);
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "registered membership changed or is incomplete",
                ));
            }
            let (retained, mut reservation) =
                self.prepared_root_facts(&root, &view["classifier"], context, &remaining, cancel)?;
            let shared = members
                .iter()
                .position(|source| source.stamp.identity.file() == root.stamp.identity.file());
            let (before, after) = match shared {
                Some(index) => (&members[..index], &members[index + 1..]),
                None => (&members[..], &members[members.len()..]),
            };
            let others = || before.iter().chain(after);
            let graph_id = self.token("prepared-graph");
            let stamps_bytes = (others().count() + 1) * size_of::<(PathBuf, SourceStamp)>()
                + root.canonical_path.as_os_str().len()
                + others()
                    .map(|source| source.path.as_os_str().len())
                    .sum::<usize>();
            let buffers: usize = if shared.is_some() {
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
            let sources: Arc<[PreparedSourceRef]> = if shared.is_some() {
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
            return self.publish_prepared_graph(graph_id, graph, revision, context, &mut reservation);
        }
        let (retained, mut reservation) =
            self.prepared_root_facts(&root, &view["classifier"], context, &remaining, cancel)?;
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
        let build = PreparedBuild {
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

    fn prepare_graph_step(
        &self,
        token: &str,
        mut build: PreparedBuild,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(build.remaining.deadline_unix_ms)?;
        let context = &build.context;
        let (root, description) = self.pin_scope_for_work(
            &build.root_handle,
            context,
            build.remaining.deadline_unix_ms,
        )?;
        if root.stamp != build.root.stamp
            || !classifier_eq(&build.classifier, &description["classifier"])?
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared graph root generation changed",
            ));
        }
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
                build.tasks.push(GraphTask::Visit {
                    path,
                    depth: 1,
                    spawned_by: None,
                });
            }
            build.tasks.push(GraphTask::List {
                parent: build.root.canonical_path.clone(),
                depth: 1,
            });
        }
        self.extend_projection_reservation(reservation, context, FILESYSTEM_PATH_BYTES)?;
        let mut examined = 0usize;
        while examined < 8 {
            cancel.check(build.remaining.deadline_unix_ms)?;
            if let Some(mut listing) = build.listing.take() {
                let mut complete = false;
                while examined < 8 {
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
                    let stamp = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
                    self.extend_projection_reservation(
                        reservation,
                        context,
                        canonical.capacity() + READ_DIR_HANDLE_BYTES + directory.as_os_str().len(),
                    )?;
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
                    let stamp = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
                    if build.seen.contains(&stamp.identity.file()) {
                        continue;
                    }
                    if build.seen.len() >= build.remaining.max_sources {
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
        if state.prepared_builds.len() >= self.lease_cap(&build.context)? {
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

    fn collect_located(
        page: &Value,
        located: &mut HashMap<String, PathBuf>,
    ) -> Result<(), SnapshotError> {
        for item in page["sessions"]
            .as_array()
            .ok_or_else(|| invalid("invalid registered location result"))?
        {
            match str_field(item, "status")? {
                "ok" => {
                    located.insert(
                        str_field(item, "session_id")?.to_owned(),
                        PathBuf::from(str_field(item, "path")?),
                    );
                }
                "missing" => {}
                "incomplete" => {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "registered location incomplete",
                    ));
                }
                _ => return Err(invalid("invalid registered location status")),
            }
        }
        Ok(())
    }

    fn build_warm_membership(
        &self,
        key: &str,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
        remaining: &mut WorkLimits,
    ) -> Result<(WarmMembership, ProjectionReservation<'_>), SnapshotError> {
        use std::fmt::Write as _;

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
        let mut located = HashMap::new();
        if !ids.is_empty() {
            let location = json!({"schema":SCHEMA,"id":"warm-registered-locate","operation":"locate","session_ids":ids,"roots":roots,"deadline_unix_ms":remaining.deadline_unix_ms,"limits":remaining.to_json()});
            let before = usage[17];
            let mut outcome = self.locate(&location, context, cancel, usage)?;
            loop {
                let page = Self::collect_located(&outcome.0, &mut located);
                let Some(cursor) = outcome.1.take() else {
                    page?;
                    if outcome.2.is_some() {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "registered location incomplete",
                        ));
                    }
                    break;
                };
                let resumed = page.and_then(|()| {
                    self.dispatch(
                        &json!({"schema":SCHEMA,"id":"warm-registered-locate-resume","operation":"resume","cursor":&cursor}),
                        context,
                        cancel,
                        usage,
                    )
                });
                outcome = match resumed {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.lock_state().locates.remove(&cursor);
                        return Err(error);
                    }
                };
            }
            remaining.max_discovery_entries = remaining
                .max_discovery_entries
                .saturating_sub((usage[17] - before) as usize);
        }
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
            if members.len() > remaining.max_sources {
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "registered source bound exhausted",
                ));
            }
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
        let mut digest = Sha256::new();
        for source in &members {
            digest.update(source.path.as_os_str().as_encoded_bytes());
            digest.update(source.stamp.revision().as_bytes());
        }
        self.extend_projection_reservation(
            &mut reservation,
            context,
            arc_slice_bytes::<PreparedSourceRef>(members.len())
                + arc_slice_bytes::<(PathBuf, Option<SourceStamp>)>(sidechain_dirs.len()),
        )?;
        let mut revision = String::with_capacity(revision_len);
        write!(revision, "{:x}", digest.finalize()).expect("hex digest");
        assert_eq!(revision.capacity(), revision_len);
        Ok((
            WarmMembership {
                members: members.into(),
                sidechain_dirs: sidechain_dirs.into(),
                revision,
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
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut remaining = limits(request)?;
        if context["work_class"].as_str() != Some("background") {
            return Err(invalid("registered warming requires background work class"));
        }
        if request["classifier"] != json!({"id":"native","version":"1"}) {
            return Err(invalid("registered warming requires the native classifier"));
        }
        let ids = request["thread_ids"]
            .as_array()
            .ok_or_else(|| invalid("missing registered thread ids"))?;
        let roots = request["roots"]
            .as_array()
            .ok_or_else(|| invalid("missing registered roots"))?;
        let direct = request["direct_paths"]
            .as_array()
            .ok_or_else(|| invalid("missing registered direct paths"))?;
        if ids.len() > 1024 || roots.len() > 64 || direct.len() > 1024 {
            return Err(invalid("registered warming input exceeds its bound"));
        }
        let start = number(request, "start_index")?;
        let key = Self::warm_membership_key(request, context)?;
        let cached = self.lock_state()
            .warm_memberships
            .get(&key)
            .cloned();
        let (membership, mut reservation) = match cached {
            Some(cached)
                if start > 0
                    || self.validate_warm_membership(
                        &cached.members,
                        &cached.sidechain_dirs,
                        context,
                        cancel,
                        remaining.deadline_unix_ms,
                    )? =>
            {
                (cached, None)
            }
            Some(_) => {
                self.lock_state().remove_warm_membership(&key);
                let (membership, reservation) =
                    self.build_warm_membership(&key, request, context, cancel, usage, &mut remaining)?;
                (membership, Some(reservation))
            }
            None => {
                let (membership, reservation) =
                    self.build_warm_membership(&key, request, context, cancel, usage, &mut remaining)?;
                (membership, Some(reservation))
            }
        };
        if !membership.complete {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered membership is incomplete",
            ));
        }
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
        let mut next = start;
        let mut steps = 0usize;
        'warming: while next < members.len() && steps < 8 {
            cancel.check(remaining.deadline_unix_ms)?;
            let source = &members[next];
            let before_read = usage[1];
            let before_events = usage[3];
            let (stamp, mut outcome) = match self.prepared_source(
                &source.path,
                context,
                &mut remaining,
                cancel,
                usage,
            ) {
                Ok(outcome) => outcome,
                Err(error)
                    if error.status == Status::Incomplete && error.reason == "source_read_limit" =>
                {
                    break 'warming;
                }
                Err(error)
                    if error.status == Status::Deadline
                        && (usage[1] > before_read || usage[3] > before_events) =>
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
            loop {
                match outcome {
                    PreparedSourceOutcome::Ready { stamp, .. } => {
                        if stamp != source.stamp {
                            return Err(SnapshotError::new(
                                Status::Changed,
                                "registered source changed",
                            ));
                        }
                        next += 1;
                        break;
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
                            context,
                            &mut remaining,
                            cancel,
                            usage,
                        ) {
                            Ok(outcome) => outcome,
                            Err(error)
                                if error.status == Status::Incomplete
                                    && error.reason == "source_read_limit"
                                    || error.status == Status::SourceLimit
                                        && error.reason == "cumulative preparation work budget exhausted"
                                    || error.status == Status::Deadline
                                        && (usage[1] > before_read
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
            }
            steps += 1;
        }
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
            for source in members.iter() {
                cancel.check(remaining.deadline_unix_ms)?;
                let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
                    source.stamp,
                    str_field(context, "registry_generation")?,
                    str_field(context, "admission")?,
                    &context["authority"],
                    &request["classifier"],
                )?;
                if !self.prepared_disk.has_entry(&key)? {
                    return Err(SnapshotError::new(
                        Status::RetainedLimit,
                        "prepared facts cache cannot retain complete membership",
                    ));
                }
            }
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
        usage: &mut [u64; 18],
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
        let (root, description) = self.pin_scope_for_work(&handle, context, deadline)?;
        let current = SourceStamp::of(&std::fs::metadata(&root.canonical_path).map_err(|_| {
            SnapshotError::new(Status::Changed, "prepared graph root disappeared")
        })?)
        .viewed_as(stamp);
        if root.stamp != stamp
            || current != stamp && !self.extends_prefix(&root, remaining, usage)?
            || !classifier_eq(&classifier, &description["classifier"])?
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
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let page = self.prepared_query_steps(&mut cursor, context, reservation, cancel, usage);
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
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
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
            self.authority(context, Some(&source.path))?;
            let metadata = std::fs::metadata(&source.path).map_err(io_error)?;
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
                match self.resume_prepared_source(
                    &pending,
                    context,
                    &mut cursor.remaining,
                    cancel,
                    usage,
                ) {
                    Ok(PreparedSourceOutcome::Pending(next)) => {
                        pending.token = next;
                        cursor.pending = Some(pending);
                        steps += 1;
                        uncached_steps += 1;
                        continue;
                    }
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
                    &source.path,
                    context,
                    &mut cursor.remaining,
                    cancel,
                    usage,
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
            } else if facts.query(&cursor.query)?["value"].as_bool() == Some(true) {
                let data = json!({"kind":"scalar","value":true});
                encoded_size(&data, output_limit)?;
                return Ok(QueryPage::Complete(data));
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
                json!({"kind":"scalar","value":false})
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
        usage: &mut [u64; 18],
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
        let selectors = request
            .get("selectors")
            .ok_or_else(|| invalid("missing graph selectors"))?;
        let expires = graph.expires;
        let root = Arc::clone(&graph.root);
        let selector_key = (!selectors.as_array().is_some_and(|items| items.is_empty()))
            .then(|| sonic_rs::to_string(selectors).map_err(|error| invalid(error.to_string())))
            .transpose()?;
        let cached_slice = match &selector_key {
            None => Some(Arc::clone(&graph.root_facts)),
            Some(key) => {
                if graph.root_slices.len() >= 64 && !graph.root_slices.contains_key(key) {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "prepared root selector cache exhausted",
                    ));
                }
                graph.root_slices.get(key).cloned()
            }
        };
        drop(graph);
        let root_facts = match (cached_slice, selector_key) {
            (Some(facts), _) => facts,
            (None, Some(key)) => {
                let mut fact_limits = bounds;
                fact_limits.max_read_bytes = self.config.source;
                fact_limits.max_events = root.event_count;
                let bound = crate::snapshot_projection::facts_bound(&root);
                let mut slice_reservation = self.reserve_projection(context, bound)?;
                #[cfg(test)]
                self.fact_builds.fetch_add(1, Ordering::Relaxed);
                let (facts, _, _) = crate::snapshot_projection::prepare_facts(
                    &root,
                    selectors,
                    &fact_limits,
                    cancel,
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
                    context,
                    &mut slice_reservation,
                )?
            }
            (None, None) => unreachable!("the root facts are always cached"),
        };
        if kind != "deep_predicate_inputs" && root_facts.query(query)?["value"].as_bool() == Some(true) {
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
        let cursor = PreparedQueryCursor {
            claimant: claimant.to_owned(),
            graph_id: token.to_owned(),
            query: query.clone(),
            pending: None,
            input_records,
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

    fn publish_root_slice(
        &self,
        token: &str,
        shared: &Arc<Mutex<PreparedGraph>>,
        key: String,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Arc<crate::snapshot_prepared::PreparedFacts>, SnapshotError> {
        let mut state = self.lock_state();
        if !state.prepared_graphs.contains_key(token) {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "prepared graph expired",
            ));
        }
        let mut graph = shared.lock().expect("prepared graph");
        if let Some(existing) = graph.root_slices.get(&key) {
            return Ok(Arc::clone(existing));
        }
        if graph.root_slices.len() >= 64 {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "prepared root selector cache exhausted",
            ));
        }
        let retained = state.ledger.shared.admission([facts_anchor(&facts)])
            + key.capacity()
            + graph.root_slices.growth_for(&key);
        let covered = retained.min(reservation.bytes);
        self.admit_memory(&mut state, context, retained - covered)?;
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.insert_root_slice(&mut graph, key, Arc::clone(&facts));
        drop(graph);
        drop(state.prepared_graphs.get_mut(token));
        Ok(facts)
    }
}

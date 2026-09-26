impl NativeStore {
    fn release_prepared_build_state(state: &mut StoreState, build: &PreparedBuild) {
        if let Some(token) = &build.location_cursor {
            state.locates.remove(token);
        }
    }

    fn prepared_fact_bytes(state: &StoreState) -> usize {
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let mut add = |facts: &Arc<crate::snapshot_prepared::PreparedFacts>| {
            if seen.insert(Arc::as_ptr(facts) as usize) {
                bytes += facts.accounted_bytes();
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
        bytes
    }

    fn cache_prepared_facts(
        &self,
        stamp: SourceStamp,
        facts: Arc<crate::snapshot_prepared::PreparedFacts>,
        classifier: &Value,
        context: &Value,
    ) -> Result<Arc<crate::snapshot_prepared::PreparedFacts>, SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let authority = &context["authority"];
        let mut state = self.state.lock().expect("snapshot state");
        if let Some(cached) = state.prepared_facts.get_mut(&stamp.identity) {
            if cached.stamp == stamp
                && cached.registry_generation == registry_generation
                && cached.admission == admission
                && cached.authority == *authority
                && cached.classifier == *classifier
            {
                cached.last_used = now_ms();
                return Ok(Arc::clone(&cached.facts));
            }
        }
        state.prepared_facts.remove(&stamp.identity);
        let accounted = facts.accounted_bytes();
        let budget = self.config.retained.min(self.config.prepared_fact_memory);
        let mut used = Self::prepared_fact_bytes(&state);
        while accounted > budget.saturating_sub(used) {
            let oldest = state
                .prepared_facts
                .iter()
                .filter(|(_, cached)| Arc::strong_count(&cached.facts) == 1)
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(identity, _)| *identity);
            let Some(oldest) = oldest else {
                return Err(SnapshotError::new(
                    Status::RetainedLimit,
                    "prepared facts cache budget exhausted",
                ));
            };
            state.prepared_facts.remove(&oldest).expect("cached fact");
            used = Self::prepared_fact_bytes(&state);
        }
        self.admit_memory(&mut state, context, accounted)?;
        state.prepared_facts.insert(
            stamp.identity,
            CachedPreparedFacts {
                stamp,
                registry_generation: registry_generation.to_owned(),
                admission: admission.to_owned(),
                authority: authority.clone(),
                classifier: classifier.clone(),
                facts: Arc::clone(&facts),
                last_used: now_ms(),
            },
        );
        Ok(facts)
    }

    fn prepared_root_facts(
        &self,
        root: &Arc<TranscriptSnapshot>,
        classifier: &Value,
        context: &Value,
        remaining: &WorkLimits,
        cancel: &Cancellation,
    ) -> Result<Arc<crate::snapshot_prepared::PreparedFacts>, SnapshotError> {
        let registry_generation = str_field(context, "registry_generation")?;
        if let Some(facts) = {
            let mut state = self.state.lock().expect("snapshot state");
            state
                .prepared_facts
                .get_mut(&root.stamp.identity)
                .and_then(|cached| {
                    (cached.stamp == root.stamp
                        && cached.registry_generation == registry_generation
                        && cached.admission == context["admission"].as_str().unwrap_or("")
                        && cached.authority == context["authority"]
                        && cached.classifier == *classifier)
                        .then(|| {
                            cached.last_used = now_ms();
                            Arc::clone(&cached.facts)
                        })
                })
        } {
            return Ok(facts);
        }
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            root.stamp,
            registry_generation,
            str_field(context, "admission")?,
            &context["authority"],
            classifier,
        )?;
        match self.prepared_disk.lookup(&key)? {
            crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                let facts = Arc::new(facts);
                return match self.cache_prepared_facts(
                    root.stamp,
                    Arc::clone(&facts),
                    classifier,
                    context,
                ) {
                    Ok(cached) => Ok(cached),
                    Err(error) if error.status == Status::RetainedLimit => Ok(facts),
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
        let mut fact_limits = *remaining;
        fact_limits.max_read_bytes = self.config.source;
        fact_limits.max_events = root.event_count;
        let (facts, _, _) =
            crate::snapshot_projection::prepare_facts(root, &json!([]), &fact_limits, cancel)?;
        self.prepared_disk.insert(&key, &facts)?;
        let facts = Arc::new(facts);
        match self.cache_prepared_facts(root.stamp, Arc::clone(&facts), classifier, context) {
            Ok(cached) => Ok(cached),
            Err(error) if error.status == Status::RetainedLimit => Ok(facts),
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
    ) -> Result<(SourceStamp, PreparedSourceOutcome), SnapshotError> {
        cancel.check(remaining.deadline_unix_ms)?;
        let canonical = std::fs::canonicalize(path).map_err(io_error)?;
        self.authority(context, Some(&canonical))?;
        let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("prepared graph source must be a file"));
        }
        let stamp = SourceStamp::of(&metadata);
        {
            let mut state = self.state.lock().expect("snapshot state");
            if let Some((slot, touched)) = state.prepared_loads.get_mut(&stamp.identity) {
                if slot.stamp == stamp {
                    slot.deadline.store(
                        now_ms().saturating_add(self.config.preparation),
                        Ordering::Release,
                    );
                    *touched = now_ms();
                } else {
                    state.prepared_loads.remove(&stamp.identity);
                    state.loads.remove(&stamp.identity);
                }
            }
        }
        let registry_generation = str_field(context, "registry_generation")?;
        if let Some(facts) = {
            let mut state = self.state.lock().expect("snapshot state");
            state
                .prepared_facts
                .get_mut(&stamp.identity)
                .and_then(|cached| {
                    (cached.stamp == stamp
                        && cached.registry_generation == registry_generation
                        && cached.admission == context["admission"].as_str().unwrap_or("")
                        && cached.authority == context["authority"]
                        && cached.classifier == json!({"id":"native","version":"1"}))
                    .then(|| {
                        cached.last_used = now_ms();
                        Arc::clone(&cached.facts)
                    })
                })
        } {
            usage[7] += 1;
            return Ok((stamp, PreparedSourceOutcome::Ready(stamp, facts)));
        }
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            registry_generation,
            str_field(context, "admission")?,
            &context["authority"],
            &json!({"id":"native","version":"1"}),
        )?;
        match self.prepared_disk.lookup(&key)? {
            crate::snapshot_prepared_disk::DiskLookup::Hit(facts) => {
                usage[7] += 1;
                return Ok((
                    stamp,
                    PreparedSourceOutcome::Ready(stamp, Arc::new(facts)),
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
        if self.config.read_step.min(stamp.size as usize) > remaining.max_read_bytes {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "prepared graph read budget exhausted",
            ));
        }
        let acquire = json!({"schema":SCHEMA,"id":"prepare-graph-source","operation":"acquire","path":canonical.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":remaining.deadline_unix_ms,"limits":{"max_read_bytes":self.config.source,"max_events":1_000_000,"max_items":remaining.max_items,"max_output_bytes":remaining.max_output_bytes,"max_discovery_entries":remaining.max_discovery_entries,"max_sources":remaining.max_sources}});
        let before_bytes = usage[1];
        let before_events = usage[3];
        let outcome = self.acquire(&acquire, context, cancel, usage);
        {
            let mut state = self.state.lock().expect("snapshot state");
            if let Some(slot) = state.loads.get(&stamp.identity).cloned() {
                state.prepared_loads.insert(stamp.identity, (slot, now_ms()));
            }
        }
        let outcome = outcome?;
        remaining.max_read_bytes = remaining
            .max_read_bytes
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
    ) -> Result<PreparedSourceOutcome, SnapshotError> {
        if self.config.read_step.min(pending.stamp.size as usize) > remaining.max_read_bytes {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "prepared graph read budget exhausted",
            ));
        }
        self.authority(
            context,
            Some(&std::fs::canonicalize(&pending.path).map_err(io_error)?),
        )?;
        let before_bytes = usage[1];
        let before_events = usage[3];
        let outcome = self.dispatch(&json!({"schema":SCHEMA,"id":"prepare-graph-source-resume","operation":"resume","cursor":pending.token}), context, cancel, usage)?;
        remaining.max_read_bytes = remaining
            .max_read_bytes
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
    ) -> Result<PreparedSourceOutcome, SnapshotError> {
        let handle = &outcome["description"]["handle"];
        let (snapshot, _) = self.pin_scope_for_work(handle, context, remaining.deadline_unix_ms)?;
        if snapshot.stamp != stamp {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared source revision changed",
            ));
        }
        let mut fact_limits = *remaining;
        fact_limits.max_read_bytes = self.config.source;
        fact_limits.max_events = snapshot.event_count;
        let prepared =
            crate::snapshot_projection::prepare_facts(&snapshot, &json!([]), &fact_limits, cancel);
        {
            let mut state = self.state.lock().expect("snapshot state");
            state.leases.remove(str_field(handle, "lease_id")?);
            let recent_codex = snapshot.provider == Provider::Codex
                && snapshot.codex_raw.is_some()
                && (now_ms() as i128 * 1_000_000).saturating_sub(stamp.mtime_ns)
                    <= 30 * 60 * 1_000_000_000;
            if recent_codex {
                state.recent_codex.insert(stamp.identity, now_ms());
                let mut raw_bytes: usize = state
                    .recent_codex
                    .keys()
                    .filter_map(|identity| state.latest.get(identity))
                    .filter_map(|snapshot| snapshot.codex_raw.as_ref())
                    .map(|raw| raw.len())
                    .sum();
                while raw_bytes > 128 * 1024 * 1024 {
                    let oldest = state
                        .recent_codex
                        .iter()
                        .min_by_key(|(_, touched)| *touched)
                        .map(|(identity, _)| *identity)
                        .expect("raw codex cache exceeds bound");
                    state.recent_codex.remove(&oldest);
                    if let Some(snapshot) = state.latest.remove(&oldest) {
                        raw_bytes = raw_bytes.saturating_sub(
                            snapshot.codex_raw.as_ref().map_or(0, |raw| raw.len()),
                        );
                    }
                }
            } else {
                state.recent_codex.remove(&stamp.identity);
                state.latest.remove(&stamp.identity);
            }
        }
        let (facts, _, _) = prepared?;
        drop(snapshot);
        let classifier = json!({"id":"native","version":"1"});
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            str_field(context, "registry_generation")?,
            str_field(context, "admission")?,
            &context["authority"],
            &classifier,
        )?;
        self.prepared_disk.insert(&key, &facts)?;
        let facts = Arc::new(facts);
        let facts = match self.cache_prepared_facts(stamp, Arc::clone(&facts), &classifier, context) {
            Ok(cached) => cached,
            Err(error) if error.status == Status::RetainedLimit => facts,
            Err(error) => return Err(error),
        };
        self.state
            .lock()
            .expect("snapshot state")
            .prepared_loads
            .remove(&stamp.identity);
        Ok(PreparedSourceOutcome::Ready(stamp, facts))
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
        membership: &WarmMembership,
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<bool, SnapshotError> {
        Ok(self.validate_source_stamps(&membership.members, context, cancel, deadline)?
            && self.validate_sidechain_dirs(
                &membership.sidechain_dirs,
                context,
                cancel,
                deadline,
            )?)
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
            let membership = self
                .state
                .lock()
                .expect("snapshot state")
                .warm_memberships
                .get(&key)
                .cloned()
                .ok_or_else(|| {
                    SnapshotError::new(Status::Incomplete, "registered membership is not warmed")
                })?;
            if !membership.complete
                || !self.validate_warm_membership(
                    &membership,
                    context,
                    cancel,
                    remaining.deadline_unix_ms,
                )?
            {
                self.state
                    .lock()
                    .expect("snapshot state")
                    .warm_memberships
                    .remove(&key);
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "registered membership changed or is incomplete",
                ));
            }
            let root_facts =
                self.prepared_root_facts(&root, &view["classifier"], context, &remaining, cancel)?;
            let sources: Vec<_> = membership
                .members
                .into_iter()
                .filter(|source| source.stamp.identity != root.stamp.identity)
                .collect();
            let mut stamps = vec![(root.canonical_path.clone(), root.stamp)];
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
            let graph_id = self.token("prepared-graph");
            let accounted = size_of::<PreparedGraph>()
                + sources.capacity() * size_of::<PreparedSourceRef>()
                + membership.sidechain_dirs.capacity()
                    * size_of::<(PathBuf, Option<SourceStamp>)>()
                + membership
                    .sidechain_dirs
                    .iter()
                    .map(|(path, _)| path.as_os_str().len())
                    .sum::<usize>();
            let graph = PreparedGraph {
                claimant: str_field(context, "claimant")?.to_owned(),
                registry_generation: str_field(context, "registry_generation")?.to_owned(),
                admission: str_field(context, "admission")?.to_owned(),
                authority: context["authority"].clone(),
                root,
                root_handle: root_handle.clone(),
                classifier: view["classifier"].clone(),
                root_facts,
                root_slices: HashMap::new(),
                revision: revision.clone(),
                stamps,
                validated: false,
                sources,
                sidechain_dirs: membership.sidechain_dirs,
                remaining,
                expires: (now_ms() + self.config.ttl).min(remaining.deadline_unix_ms),
                accounted,
            };
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            if state.prepared_graphs.len() >= self.lease_cap(context)? {
                return Err(SnapshotError::new(
                    Status::LeaseLimit,
                    "prepared graph admission exhausted",
                ));
            }
            state
                .prepared_graphs
                .insert(graph_id.clone(), Arc::new(Mutex::new(graph)));
            return Ok((
                json!({"kind":"prepared_graph","handle":{"graph_id":graph_id,"owner_epoch":self.owner_epoch,"revision":revision,"complete":true}}),
                None,
                None,
            ));
        }
        let root_facts =
            self.prepared_root_facts(&root, &view["classifier"], context, &remaining, cancel)?;
        let build = PreparedBuild {
            claimant: str_field(context, "claimant")?.to_owned(),
            context: context.clone(),
            request: request.clone(),
            root: Arc::clone(&root),
            root_handle: root_handle.clone(),
            classifier: view["classifier"].clone(),
            root_facts,
            remaining,
            location_started: false,
            location_finished: ids.is_empty(),
            location_cursor: None,
            located: HashMap::new(),
            tasks: Vec::new(),
            listing: None,
            seen: HashSet::from([root.stamp.identity]),
            sources: Vec::new(),
            stamps: vec![(root.canonical_path.clone(), root.stamp)],
            sidechain_dirs: Vec::new(),
            expires: (now_ms() + self.config.ttl).min(remaining.deadline_unix_ms),
        };
        self.prepare_graph_step(&self.token("prepared-build"), build, cancel, usage)
    }

    fn prepare_graph_step(
        &self,
        token: &str,
        mut build: PreparedBuild,
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
        if !build.location_finished {
            let before = usage[17];
            let outcome = if let Some(cursor) = build.location_cursor.take() {
                self.dispatch(
                    &json!({"schema":SCHEMA,"id":"prepare-graph-locate-resume","operation":"resume","cursor":cursor}),
                    context,
                    cancel,
                    usage,
                )?
            } else {
                let ids = &build.request["thread_ids"];
                let roots = &build.request["roots"];
                let bounds = build.remaining;
                let location = json!({"schema":SCHEMA,"id":"prepare-graph-locate","operation":"locate","session_ids":ids,"roots":roots,"deadline_unix_ms":bounds.deadline_unix_ms,"limits":{"max_read_bytes":bounds.max_read_bytes,"max_events":bounds.max_events,"max_items":bounds.max_items,"max_output_bytes":bounds.max_output_bytes,"max_discovery_entries":bounds.max_discovery_entries,"max_sources":bounds.max_sources}});
                build.location_started = true;
                self.locate(&location, context, cancel, usage)?
            };
            build.remaining.max_discovery_entries = build
                .remaining
                .max_discovery_entries
                .saturating_sub((usage[17] - before) as usize);
            for item in outcome.0["sessions"]
                .as_array()
                .ok_or_else(|| invalid("invalid location result"))?
            {
                match str_field(item, "status")? {
                    "ok" => {
                        build.located.insert(
                            str_field(item, "session_id")?.to_owned(),
                            PathBuf::from(str_field(item, "path")?),
                        );
                    }
                    "missing" => {}
                    "incomplete" => {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "prepared registry location incomplete",
                        ))
                    }
                    _ => return Err(invalid("invalid location status")),
                }
            }
            if let Some(cursor) = outcome.1 {
                build.location_cursor = Some(cursor);
                return self.store_prepared_build(token, build);
            }
            if outcome.2.is_some() {
                return Err(SnapshotError::new(
                    Status::Incomplete,
                    "prepared registry location incomplete",
                ));
            }
            build.location_finished = true;
        }
        if !build.location_started {
            build.location_started = true;
        }
        if build.tasks.is_empty() && build.sources.is_empty() && build.listing.is_none() {
            let direct = build.request["direct_paths"]
                .as_array()
                .expect("validated direct paths");
            let mut distinct = Vec::new();
            let mut paths = HashSet::new();
            for path in direct {
                let path = path
                    .as_str()
                    .ok_or_else(|| invalid("invalid direct path"))?;
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
            let ids = build.request["thread_ids"]
                .as_array()
                .expect("validated thread ids");
            for id in ids.iter().rev() {
                if let Some(path) = build
                    .located
                    .get(id.as_str().ok_or_else(|| invalid("invalid thread id"))?)
                {
                    build.tasks.push(GraphTask::Visit {
                        path: path.clone(),
                        depth: 1,
                        spawned_by: None,
                    });
                }
            }
            build.tasks.push(GraphTask::List {
                parent: build.root.canonical_path.clone(),
                depth: 1,
            });
        }
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
                        && !entry.file_name().to_string_lossy().starts_with("._")
                    {
                        listing.children.push(path);
                    }
                }
                if !complete {
                    build.listing = Some(listing);
                    break;
                }
                listing.children.sort();
                for path in listing.children.into_iter().rev() {
                    let stem = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("");
                    let spawned_by = stem.strip_prefix("agent-").unwrap_or(stem).to_owned();
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
                    let directory = parent
                        .parent()
                        .ok_or_else(|| invalid("source has no parent"))?
                        .join(
                            parent
                                .file_stem()
                                .ok_or_else(|| invalid("source has no stem"))?,
                        )
                        .join("subagents");
                    let canonical = match std::fs::canonicalize(&directory) {
                        Ok(canonical) => canonical,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            build.sidechain_dirs.push((directory, None));
                            continue;
                        }
                        Err(error) => return Err(io_error(error)),
                    };
                    self.authority(context, Some(&canonical))?;
                    build.sidechain_dirs.push((
                        canonical.clone(),
                        Some(SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?)),
                    ));
                    build.listing = Some(GraphListing {
                        entries: std::fs::read_dir(&directory).map_err(io_error)?,
                        children: Vec::new(),
                        depth,
                    });
                }
                GraphTask::Visit {
                    path,
                    depth,
                    spawned_by: _,
                } => {
                    examined += 1;
                    let canonical = std::fs::canonicalize(&path).map_err(io_error)?;
                    self.authority(context, Some(&canonical))?;
                    let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                    if !build.seen.insert(SourceStamp::of(&metadata).identity) {
                        continue;
                    }
                    if build.seen.len() > build.remaining.max_sources {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "prepared graph source budget exhausted",
                        ));
                    }
                    let stamp = SourceStamp::of(&metadata);
                    build.stamps.push((canonical.clone(), stamp));
                    build.sources.push(PreparedSourceRef {
                        path: canonical.clone(),
                        stamp,
                    });
                    build.tasks.push(GraphTask::List {
                        parent: canonical,
                        depth: depth + 1,
                    });
                }
            }
        }
        if build.listing.is_some() || !build.tasks.is_empty() {
            return self.store_prepared_build(token, build);
        }
        let mut digest = Sha256::new();
        for (path, stamp) in &build.stamps {
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update(stamp.revision().as_bytes());
        }
        let revision = format!("{:x}", digest.finalize());
        let graph_id = self.token("prepared-graph");
        let accounted = size_of::<PreparedGraph>()
            + build.sources.capacity() * size_of::<PreparedSourceRef>()
            + build.sidechain_dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + build
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.as_os_str().len())
                .sum::<usize>();
        let graph = PreparedGraph {
            claimant: build.claimant,
            registry_generation: str_field(context, "registry_generation")?.to_owned(),
            admission: str_field(context, "admission")?.to_owned(),
            authority: context["authority"].clone(),
            root: build.root,
            root_handle: build.root_handle,
            classifier: build.classifier,
            root_facts: build.root_facts,
            root_slices: HashMap::new(),
            revision: revision.clone(),
            stamps: build.stamps,
            validated: false,
            sources: build.sources,
            sidechain_dirs: build.sidechain_dirs,
            remaining: build.remaining,
            expires: (now_ms() + self.config.ttl).min(build.remaining.deadline_unix_ms),
            accounted,
        };
        let mut state = self.state.lock().expect("snapshot state");
        if state.prepared_graphs.len() >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared graph admission exhausted",
            ));
        }
        state
            .prepared_graphs
            .insert(graph_id.clone(), Arc::new(Mutex::new(graph)));
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
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let mut state = self.state.lock().expect("snapshot state");
        if state.prepared_builds.len() >= self.lease_cap(&build.context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared build admission exhausted",
            ));
        }
        build.expires = (now_ms() + self.config.ttl).min(build.remaining.deadline_unix_ms);
        state.prepared_builds.insert(token.to_owned(), build);
        Ok((
            Value::new_null(),
            Some(token.to_owned()),
            Some("prepared graph work incomplete".to_owned()),
        ))
    }

    fn build_warm_membership(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
        remaining: &mut WorkLimits,
    ) -> Result<WarmMembership, SnapshotError> {
        let ids = request["thread_ids"]
            .as_array()
            .ok_or_else(|| invalid("missing registered thread ids"))?;
        let roots = request["roots"]
            .as_array()
            .ok_or_else(|| invalid("missing registered roots"))?;
        let direct = request["direct_paths"]
            .as_array()
            .ok_or_else(|| invalid("missing registered direct paths"))?;
        let mut located = HashMap::new();
        if !ids.is_empty() {
            let location = json!({"schema":SCHEMA,"id":"warm-registered-locate","operation":"locate","session_ids":ids,"roots":roots,"deadline_unix_ms":remaining.deadline_unix_ms,"limits":{"max_read_bytes":remaining.max_read_bytes,"max_events":remaining.max_events,"max_items":remaining.max_items,"max_output_bytes":remaining.max_output_bytes,"max_discovery_entries":remaining.max_discovery_entries,"max_sources":remaining.max_sources}});
            let before = usage[17];
            let mut outcome = self.locate(&location, context, cancel, usage)?;
            loop {
                for item in outcome.0["sessions"]
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
                if let Some(cursor) = outcome.1 {
                    outcome = self.dispatch(
                        &json!({"schema":SCHEMA,"id":"warm-registered-locate-resume","operation":"resume","cursor":cursor}),
                        context,
                        cancel,
                        usage,
                    )?;
                    continue;
                }
                if outcome.2.is_some() {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "registered location incomplete",
                    ));
                }
                break;
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
                members.push(PreparedSourceRef {
                    path: canonical,
                    stamp,
                });
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
            let directory = source
                .path
                .parent()
                .ok_or_else(|| invalid("registered source has no parent"))?
                .join(
                    source
                        .path
                        .file_stem()
                        .ok_or_else(|| invalid("registered source has no stem"))?,
                )
                .join("subagents");
            let canonical = match std::fs::canonicalize(&directory) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    sidechain_dirs.push((directory, None));
                    examined += 1;
                    continue;
                }
                Err(error) => return Err(io_error(error)),
            };
            self.authority(context, Some(&canonical))?;
            let directory_stamp = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
            sidechain_dirs.push((canonical.clone(), Some(directory_stamp)));
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
                    members.push(PreparedSourceRef {
                        path: canonical,
                        stamp,
                    });
                }
            }
            examined += 1;
        }
        let mut digest = Sha256::new();
        for source in &members {
            digest.update(source.path.as_os_str().as_encoded_bytes());
            digest.update(source.stamp.revision().as_bytes());
        }
        Ok(WarmMembership {
            members,
            sidechain_dirs,
            revision: format!("{:x}", digest.finalize()),
            complete: located.len() == ids.len(),
            expires: now_ms().saturating_add(30 * 60_000),
        })
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
        let cached = self
            .state
            .lock()
            .expect("snapshot state")
            .warm_memberships
            .get(&key)
            .cloned();
        let membership = if let Some(cached) = cached {
            if start > 0
                || self.validate_warm_membership(
                    &cached,
                    context,
                    cancel,
                    remaining.deadline_unix_ms,
                )?
            {
                cached
            } else {
                self.state
                    .lock()
                    .expect("snapshot state")
                    .warm_memberships
                    .remove(&key);
                self.build_warm_membership(request, context, cancel, usage, &mut remaining)?
            }
        } else {
            self.build_warm_membership(request, context, cancel, usage, &mut remaining)?
        };
        if !membership.complete {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered membership is incomplete",
            ));
        }
        {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            if !state.warm_memberships.contains_key(&key) {
                if state.warm_memberships.len() >= 32 {
                    let oldest = state
                        .warm_memberships
                        .iter()
                        .min_by_key(|(_, membership)| membership.expires)
                        .map(|(key, _)| key.clone())
                        .expect("full warm membership cache");
                    state.warm_memberships.remove(&oldest);
                }
                self.admit_memory(
                    &mut state,
                    context,
                    membership.accounted_bytes() + key.capacity(),
                )?;
                state.warm_memberships.insert(key.clone(), membership.clone());
            }
        }
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
        while next < members.len() && steps < 8 {
            cancel.check(remaining.deadline_unix_ms)?;
            let source = &members[next];
            let before_read = usage[1];
            match self.prepared_source(&source.path, context, &mut remaining, cancel, usage) {
                Ok((stamp, PreparedSourceOutcome::Ready(_, _))) => {
                    if stamp != source.stamp {
                        return Err(SnapshotError::new(
                            Status::Changed,
                            "registered source changed",
                        ));
                    }
                    next += 1;
                }
                Ok((_, PreparedSourceOutcome::Pending(token))) => {
                    self.state
                        .lock()
                        .expect("snapshot state")
                        .waiters
                        .remove(&token);
                    break;
                }
                Err(error)
                    if error.status == Status::Incomplete
                        && error.reason == "prepared graph read budget exhausted" =>
                {
                    break;
                }
                Err(error) if error.status == Status::Deadline && usage[1] > before_read => {
                    break;
                }
                Err(error) => return Err(error),
            }
            steps += 1;
        }
        let complete = next == members.len();
        if complete {
            if !self.validate_warm_membership(
                &membership,
                context,
                cancel,
                remaining.deadline_unix_ms,
            )? {
                self.state
                    .lock()
                    .expect("snapshot state")
                    .warm_memberships
                    .remove(&key);
                return Err(SnapshotError::new(
                    Status::Changed,
                    "registered warming membership changed",
                ));
            }
            for source in members {
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
            let load = self
                .state
                .lock()
                .expect("snapshot state")
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
        })?);
        if root.stamp != stamp
            || current != stamp
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
        cancel: &Cancellation,
        usage: &mut [u64; 18],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        if cursor.claimant != str_field(context, "claimant")? {
            return Err(SnapshotError::new(
                Status::StaleCursor,
                "prepared query claimant differs",
            ));
        }
        let graph = self
            .state
            .lock()
            .expect("snapshot state")
            .prepared_graphs
            .get(&cursor.graph_id)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleCursor, "prepared graph expired"))?;
        self.validate_prepared_root(&graph, context)?;
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
            (graph.sources.clone(), graph.sidechain_dirs.clone())
        };
        let inputs = cursor.root_record.is_some();
        let total = sources.len() + usize::from(inputs);
        let mut records = Vec::new();
        let mut bytes = 128usize;
        let mut steps = 0usize;
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
        while cursor.next < total && steps < 8 && (!inputs || records.len() < page_items) {
            cancel.check(cursor.remaining.deadline_unix_ms)?;
            if inputs && cursor.next == 0 {
                let record = cursor.root_record.as_ref().expect("input root record").clone();
                let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)? + 1;
                if bytes + record_bytes > output_limit {
                    return Err(SnapshotError::new(
                        Status::OutputLimit,
                        "prepared input record exceeds page bound",
                    ));
                }
                bytes += record_bytes;
                records.push(record);
                cursor.next += 1;
                continue;
            }
            let source = &sources[cursor.next - usize::from(inputs)];
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
                        continue;
                    }
                    Ok(ready) => ready,
                    Err(error) => {
                        self.state
                            .lock()
                            .expect("snapshot state")
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
                        cursor.pending = Some(PendingPreparedSource {
                            token: source_cursor,
                            path: source.path.clone(),
                            stamp: source.stamp,
                        });
                        steps += 1;
                        continue;
                    }
                    (_, ready) => ready,
                }
            };
            let PreparedSourceOutcome::Ready(stamp, facts) = outcome else {
                return Err(invalid("prepared source did not finish"));
            };
            if stamp != source.stamp {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "prepared source revision changed",
                ));
            }
            if inputs {
                let record = sonic_rs::to_string(&facts.inputs)
                    .map_err(|error| invalid(error.to_string()))?;
                let record_bytes = encoded_size(&json!(&record), MAX_DATA_BYTES)? + 1;
                if bytes + record_bytes > output_limit {
                    if records.is_empty() {
                        return Err(SnapshotError::new(
                            Status::OutputLimit,
                            "prepared input record exceeds page bound",
                        ));
                    }
                    break;
                }
                bytes += record_bytes;
                records.push(record);
            } else if facts.query(&cursor.query)?["value"].as_bool() == Some(true) {
                let data = json!({"kind":"scalar","value":true});
                encoded_size(&data, output_limit)?;
                return Ok((data, None, None));
            }
            cursor.next += 1;
            steps += 1;
        }
        if cursor.next == total {
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
            return Ok((data, None, None));
        }
        let data = if inputs {
            json!({"kind":"records","record_schema":"cc-transcript.predicate-inputs/1","records_json":records})
        } else {
            Value::new_null()
        };
        let encoded = encoded_size(&data, output_limit)?;
        cursor.remaining.max_output_bytes = cursor.remaining.max_output_bytes.saturating_sub(encoded);
        cursor.remaining.max_items = cursor.remaining.max_items.saturating_sub(
            data.get("records_json")
                .and_then(Value::as_array)
                .map_or(0, |items| items.len()),
        );
        let mut state = self.state.lock().expect("snapshot state");
        if !state.prepared_graphs.contains_key(&cursor.graph_id) {
            return Err(SnapshotError::new(Status::StaleCursor, "prepared graph released"));
        }
        if state.prepared_queries.len() >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "prepared query admission exhausted",
            ));
        }
        state.prepared_queries.insert(token.to_owned(), cursor);
        Ok((data, Some(token.to_owned()), Some("prepared query page incomplete".to_owned())))
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
        let graph = self
            .state
            .lock()
            .expect("snapshot state")
            .prepared_graphs
            .get(token)
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "prepared graph expired"))?;
        self.validate_prepared_root(&graph, context)?;
        let mut graph = graph.lock().expect("prepared graph");
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
            for (path, stamp) in &graph.stamps {
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
        let bounds = limits(request)?;
        cancel.check(bounds.deadline_unix_ms)?;
        let selectors = request
            .get("selectors")
            .ok_or_else(|| invalid("missing graph selectors"))?;
        let root_facts = if selectors.as_array().is_some_and(|items| items.is_empty()) {
            Arc::clone(&graph.root_facts)
        } else {
            let key = sonic_rs::to_string(selectors).map_err(|error| invalid(error.to_string()))?;
            if let Some(facts) = graph.root_slices.get(&key) {
                Arc::clone(facts)
            } else {
                if graph.root_slices.len() >= 64 {
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "prepared root selector cache exhausted",
                    ));
                }
                let mut fact_limits = bounds;
                fact_limits.max_read_bytes = self.config.source;
                fact_limits.max_events = graph.root.event_count;
                let (facts, _, _) = crate::snapshot_projection::prepare_facts(
                    &graph.root,
                    selectors,
                    &fact_limits,
                    cancel,
                )?;
                let facts = Arc::new(facts);
                graph.root_slices.insert(key, Arc::clone(&facts));
                facts
            }
        };
        if kind != "deep_predicate_inputs" && root_facts.query(query)?["value"].as_bool() == Some(true) {
            let data = json!({"kind":"scalar","value":true});
            encoded_size(&data, bounds.max_output_bytes)?;
            return Ok((data, None, None));
        }
        let root_record = if kind == "deep_predicate_inputs" {
            Some(sonic_rs::to_string(&root_facts.inputs).map_err(|error| invalid(error.to_string()))?)
        } else {
            None
        };
        let cursor = PreparedQueryCursor {
            claimant: graph.claimant.clone(),
            graph_id: token.to_owned(),
            query: query.clone(),
            pending: None,
            root_record,
            next: 0,
            page_output_bytes: bounds.max_output_bytes.min(MAX_DATA_BYTES),
            remaining: bounds,
            expires: graph.expires,
        };
        drop(graph);
        self.prepared_query_page(&self.token("prepared-query"), cursor, context, cancel, usage)
    }
}

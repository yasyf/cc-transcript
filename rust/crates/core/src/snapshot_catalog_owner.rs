enum CatalogCell {
    Empty,
    Idle(SourceCatalog),
    Busy,
}

struct CatalogSlot {
    cell: Mutex<CatalogCell>,
    accounted: AtomicUsize,
}

impl Charge<String> for Arc<CatalogSlot> {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        arc_bytes::<CatalogSlot>() + MUTEX_STORAGE_BYTES + self.accounted.load(Ordering::Acquire)
    }
}

struct CatalogProbe {
    chunks: Vec<HashSet<String>>,
    next_id: usize,
    use_hints: bool,
    epoch: u64,
    invalid_chunk: Option<usize>,
    owned_bytes: usize,
}

impl CatalogProbe {
    fn new(store: &NativeStore, ids: &[Value], chunk_size: usize, context: &Value,
        reservation: &mut ProjectionReservation<'_>) -> Result<Self, SnapshotError> {
        if chunk_size == 0 { return Err(invalid("location request exceeds session item bound")); }
        NativeStore::validate_catalog_chunk(&ids[..ids.len().min(chunk_size)])?;
        let count = ids.len().div_ceil(chunk_size);
        store.extend_projection_reservation(reservation, context,
            count * size_of::<HashSet<String>>()
                + ids.chunks(chunk_size).map(|chunk| set_growth(&HashSet::<String>::new(), chunk.len())).sum::<usize>()
                + ids.iter().filter_map(Value::as_str).map(str::len).sum::<usize>())?;
        let mut chunks = Vec::with_capacity(count);
        let mut owned_bytes = chunks.capacity() * size_of::<HashSet<String>>();
        let mut invalid_chunk = None;
        for (index, chunk) in ids.chunks(chunk_size).enumerate() {
            let mut wanted = HashSet::with_capacity(chunk.len());
            for value in chunk {
                let Some(id) = value.as_str().filter(|id| !id.is_empty() && id.len() <= 256) else {
                    invalid_chunk = Some(index); break;
                };
                if !wanted.insert(id.to_owned()) { invalid_chunk = Some(index); break; }
            }
            if invalid_chunk == Some(0) { return Err(invalid("invalid or duplicate session id")); }
            owned_bytes += wanted.capacity() * size_of::<String>() + wanted.iter().map(String::capacity).sum::<usize>();
            chunks.push(wanted);
            if invalid_chunk.is_some() { break; }
        }
        Ok(Self { chunks, next_id: 0, use_hints: true, epoch: 0, invalid_chunk, owned_bytes })
    }
}

struct CatalogInput<'a> {
    ids: &'a [Value],
    roots: &'a [Value],
}

enum CatalogMode<'a> { Index(&'a mut CatalogProbe), Locate }

enum CatalogWorkResult { Indexed, Paths(HashMap<String, PathBuf>), Pending }

enum CatalogResolution<'a> {
    Indexed,
    Ready {
        paths: HashMap<String, PathBuf>,
        reservation: ProjectionReservation<'a>,
    },
    Pending(Arc<CatalogSlot>),
}

struct RegisteredBuild {
    claimant: String,
    request: Value,
    context: Value,
    request_bytes: usize,
    context_bytes: usize,
    probe: CatalogProbe,
    root: Arc<TranscriptSnapshot>,
    catalog: Option<Arc<CatalogSlot>>,
    remaining: WorkLimits,
    expires: u64,
}

impl Charge<String> for RegisteredBuild {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        size_of::<Self>() + self.claimant.capacity()
            + self.request_bytes + self.context_bytes + self.probe.owned_bytes
    }
}

fn catalog_canonical_scratch(value: &Value) -> usize {
    if let Some(object) = value.as_object() {
        let entries = object.len() * size_of::<(&str, &Value)>();
        map_growth(&HashMap::<&str, &Value>::new(), object.len()) + entries
            + object.iter().map(|(_, value)| catalog_canonical_scratch(value)).max().unwrap_or(0)
    } else if let Some(array) = value.as_array() {
        array.iter().map(catalog_canonical_scratch).max().unwrap_or(0)
    } else {
        0
    }
}

impl NativeStore {
    fn validate_catalog_chunk(ids: &[Value]) -> Result<(), SnapshotError> {
        for (index, value) in ids.iter().enumerate() {
            let id = value.as_str().ok_or_else(|| invalid("invalid session id"))?;
            if id.is_empty() || id.len() > 256 || ids[..index].iter().any(|prior| prior.as_str() == Some(id)) {
                return Err(invalid("invalid or duplicate session id"));
            }
        }
        Ok(())
    }

    fn catalog_key<'a>(
        &'a self,
        roots: &[Value],
        context: &Value,
    ) -> Result<(String, ProjectionReservation<'a>), SnapshotError> {
        let wrapper_scratch = map_growth(&HashMap::<&str, &Value>::new(), 2)
            + 2 * size_of::<(&str, &Value)>();
        let scratch = wrapper_scratch + catalog_canonical_scratch(&context["authority"])
            .max(roots.iter().map(catalog_canonical_scratch).max().unwrap_or(0));
        let mut reservation = self.reserve_projection(context,
            LOCATE_USER_PATH_SLOTS + 2 * value_bytes(&context["authority"])
                + 2 * (roots.len() * size_of::<Value>() + roots.iter().map(value_bytes).sum::<usize>()) + 1024 + scratch)?;
        let binding = json!({"authority":context["authority"],"roots":roots});
        let bytes = crate::ids::canonical_json_size(&binding).map_err(invalid)?;
        self.extend_projection_reservation(&mut reservation, context, bytes)?;
        self.before_decode("catalog_key");
        let mut encoded = String::with_capacity(bytes);
        crate::ids::write_canonical(&binding, &mut encoded).map_err(invalid)?;
        assert_eq!(encoded.len(), bytes, "canonical catalog key size");
        assert_eq!(encoded.capacity(), bytes, "canonical catalog key capacity");
        Ok((format!("{:x}", Sha256::digest(encoded.as_bytes())), reservation))
    }

    fn catalog_access<'a>(
        &'a self,
        input: CatalogInput<'_>,
        context: &Value,
        cancel: &Cancellation,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
        mut mode: CatalogMode<'_>,
    ) -> Result<CatalogResolution<'a>, SnapshotError> {
        let CatalogInput { ids, roots } = input;
        if ids.is_empty() {
            return Ok(CatalogResolution::Ready { paths: HashMap::new(), reservation: ProjectionReservation { store: self, bytes: 0 } });
        }
        if roots.is_empty() || roots.len() > 64 {
            return Err(invalid("invalid root count"));
        }
        let chunk_size = 1024.min(remaining.max_items);
        if chunk_size == 0 { return Err(invalid("location request exceeds session item bound")); }
        if matches!(mode, CatalogMode::Locate) { Self::validate_catalog_chunk(&ids[..ids.len().min(chunk_size)])?; }
        let (key, mut reservation) = self.catalog_key(roots, context)?;
        let slot = {
            let mut state = self.lock_state();
            if let Some(slot) = state.catalogs.get(&key) {
                let slot = Arc::clone(slot);
                state.touch_catalog_owner(&slot);
                slot
            } else {
                let bytes = key.capacity() + arc_bytes::<CatalogSlot>() + MUTEX_STORAGE_BYTES;
                let additional = bytes + state.catalogs.growth_for(&key);
                self.extend_projection_reservation_in(&mut state, &mut reservation, context, additional)?;
                let slot = Arc::new(CatalogSlot {
                    cell: Mutex::new(CatalogCell::Empty),
                    accounted: AtomicUsize::new(0),
                });
                let owner_growth = state.catalog_owner_growth(&key, &slot);
                self.extend_projection_reservation_in(&mut state, &mut reservation, context, owner_growth)?;
                state.catalogs.reserve_for(&key);
                state.catalogs.insert(key.clone(), Arc::clone(&slot));
                state.attach_catalog_owner(&key, &slot);
                state.transient_bytes -= additional + owner_growth;
                reservation.bytes -= additional + owner_growth;
                slot
            }
        };
        let mut catalog = {
            let mut state = self.lock_state();
            loop {
                cancel.check(remaining.deadline_unix_ms)?;
                let mut cell = slot.cell.lock().expect("source catalog");
                if matches!(*cell, CatalogCell::Busy) {
                    drop(cell);
                    #[cfg(test)]
                    {
                        let hook = self.catalog_wait_hook.lock().expect("catalog wait hook").clone();
                        if let Some(hook) = hook { hook(); }
                    }
                    state = self.fact_read_released.wait_timeout(state, FACT_READ_WAIT_SLICE).expect("source catalog").0;
                    continue;
                }
                break match std::mem::replace(&mut *cell, CatalogCell::Busy) {
                    CatalogCell::Empty => None,
                    CatalogCell::Idle(catalog) => Some(catalog),
                    CatalogCell::Busy => unreachable!("claimed source catalog"),
                };
            }
        };
        let old_bytes = slot.accounted.load(Ordering::Acquire);
        let outcome = (|| {
            let mut budget = CatalogBudget { store: self, context, reservation: &mut reservation, attempt: self.sequence.fetch_add(1, Ordering::Relaxed) as u64 };
            if catalog.is_none() {
                catalog = Some(SourceCatalog::new(self, roots, context, &mut budget)?);
            }
            let catalog = catalog.as_mut().expect("source catalog");
            match &mut mode {
                CatalogMode::Index(probe) => {
                    catalog.check_frontier(self, cancel, remaining.deadline_unix_ms, &mut budget)?;
                    if catalog.geometry_ready(self, ids, chunk_size, probe) { return Ok(CatalogWorkResult::Indexed); }
                    let before = usage[17];
                    catalog.advance(self, cancel, remaining.deadline_unix_ms,
                        self.config.event_step.min(remaining.max_discovery_entries), usage, &mut budget)?;
                    remaining.max_discovery_entries = remaining.max_discovery_entries.saturating_sub((usage[17] - before) as usize);
                    catalog.check_frontier(self, cancel, remaining.deadline_unix_ms, &mut budget)?;
                    if catalog.geometry_ready(self, ids, chunk_size, probe) { return Ok(CatalogWorkResult::Indexed); }
                    if remaining.max_discovery_entries == 0 { return Err(SnapshotError::new(Status::Incomplete, "registered location incomplete")); }
                    return Ok(CatalogWorkResult::Pending);
                }
                CatalogMode::Locate => {}
            }
            let mut paths = HashMap::new();
            let mut plan = None;
            let mut plan_epoch = 0;
            for chunk in ids.chunks(chunk_size) {
                Self::validate_catalog_chunk(chunk)?;
                let mut probe = CatalogProbe::new(self, chunk, chunk.len(), context, budget.reservation)?;
                loop {
                    catalog.check_frontier(self, cancel, remaining.deadline_unix_ms, &mut budget)?;
                    if plan_epoch != catalog.epoch { plan = None; }
                    if catalog.geometry_ready(self, chunk, chunk.len(), &mut probe) {
                        if plan.is_none() {
                            catalog.refresh(self, cancel, remaining.deadline_unix_ms, &mut budget)?;
                            if !catalog.geometry_ready(self, chunk, chunk.len(), &mut probe) { continue; }
                            catalog.sync_routes(&mut budget)?;
                            plan = Some(catalog.read_plan(cancel, remaining.deadline_unix_ms, &mut budget)?);
                            plan_epoch = catalog.epoch;
                        }
                        if let Some(located) = catalog.try_locate(self, chunk, context, cancel,
                            remaining.deadline_unix_ms, plan.as_ref().expect("catalog read plan"), &mut budget)?
                        {
                            self.remember_locations(located.iter(), context);
                            budget.reserve(map_growth(&paths, located.len()))?;
                            paths.extend(located);
                            break;
                        }
                    }
                    plan = None;
                    probe.use_hints = false;
                    probe.next_id = 0;
                    let before = usage[17];
                    catalog.advance(self, cancel, remaining.deadline_unix_ms,
                        self.config.event_step.min(remaining.max_discovery_entries), usage, &mut budget)?;
                    remaining.max_discovery_entries = remaining.max_discovery_entries.saturating_sub((usage[17] - before) as usize);
                    if remaining.max_discovery_entries == 0 && !catalog.complete {
                        return Err(SnapshotError::new(Status::Incomplete, "registered location incomplete"));
                    }
                }
            }
            catalog.refresh(self, cancel, remaining.deadline_unix_ms, &mut budget)?;
            if plan_epoch != catalog.epoch {
                return Err(SnapshotError::new(Status::Changed, "registered catalog changed during lookup"));
            }
            cancel.check(remaining.deadline_unix_ms)?;
            Ok(CatalogWorkResult::Paths(paths))
        })();
        {
            let mut state = self.lock_state();
            let new_bytes = catalog.as_ref().map_or(0, |catalog| catalog.charge() - size_of::<SourceCatalog>());
            let added = new_bytes.saturating_sub(old_bytes);
            assert!(added <= reservation.bytes, "source catalog outgrew its admitted allocation");
            state.transient_bytes -= added;
            reservation.bytes -= added;
            let record = state.catalogs.get_mut(&key).expect("source catalog owner");
            record.accounted.store(new_bytes, Ordering::Release);
            drop(record);
            *slot.cell.lock().expect("source catalog") = match catalog {
                Some(catalog) => CatalogCell::Idle(catalog),
                None => CatalogCell::Empty,
            };
        }
        self.fact_read_released.notify_all();
        match outcome? {
            CatalogWorkResult::Paths(paths) => Ok(CatalogResolution::Ready { paths, reservation }),
            CatalogWorkResult::Indexed => Ok(CatalogResolution::Indexed),
            CatalogWorkResult::Pending => Ok(CatalogResolution::Pending(slot)),
        }
    }
}

impl NativeStore {
    #[cfg(test)]
    fn audit_catalog_bytes(key: &String, slot: &Arc<CatalogSlot>) -> usize {
        let cell = slot.cell.lock().expect("source catalog");
        let heap = match &*cell {
            CatalogCell::Empty => 0,
            CatalogCell::Idle(catalog) => catalog.audit_charge() - size_of::<SourceCatalog>(),
            CatalogCell::Busy => panic!("catalog ownership audit requires a settled request"),
        };
        key.capacity() + arc_mirror::<CatalogSlot>() + MUTEX_STORAGE_MIRROR + heap
    }

    #[cfg(test)]
    fn audit_registered_build_bytes(key: &String, build: &RegisteredBuild) -> usize {
        key.capacity() + size_of::<RegisteredBuild>() + build.claimant.capacity()
            + value_bytes(&build.request) + value_bytes(&build.context)
            + build.probe.chunks.capacity() * size_of::<HashSet<String>>()
            + build.probe.chunks.iter().map(|chunk| chunk.capacity() * size_of::<String>() + chunk.iter().map(String::capacity).sum::<usize>()).sum::<usize>()
    }

    fn prepare_graph(
        &self, request: &Value, context: &Value, cancel: &Cancellation, usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        if request.get("thread_ids").and_then(Value::as_array).is_some_and(|ids| ids.is_empty()) {
            return self.prepare_graph_ready(request, context, cancel, usage);
        }
        let remaining = limits(request)?;
        let view = request.get("view").ok_or_else(|| invalid("missing view"))?;
        if !view["attachments"].as_array().is_some_and(|attachments| attachments.is_empty()) {
            return Err(invalid("prepare_graph accepts registry identities, not attachments"));
        }
        let handle = view.get("handle").ok_or_else(|| invalid("missing root handle"))?;
        let scope = self.renew_scope_for_work(handle, context, remaining.deadline_unix_ms)?;
        if !classifier_eq(&view["classifier"], &scope.classifier)? {
            return Err(invalid("prepared graph classifier differs from root"));
        }
        let ids = request.get("thread_ids").and_then(Value::as_array).ok_or_else(|| invalid("missing thread_ids"))?;
        let roots = request.get("roots").and_then(Value::as_array).ok_or_else(|| invalid("missing roots"))?;
        request.get("direct_paths").and_then(Value::as_array).ok_or_else(|| invalid("missing direct_paths"))?;
        if roots.len() > 64 { return Err(invalid("prepared registry input exceeds its bound")); }
        let mut reservation = self.reserve_projection(context,
            size_of::<RegisteredBuild>() + value_bytes(request) + value_bytes(context)
                + str_field(context, "claimant")?.len())?;
        let probe = CatalogProbe::new(self, ids, 1024.min(remaining.max_items), context, &mut reservation)?;
        let mut build = RegisteredBuild {
            claimant: str_field(context, "claimant")?.to_owned(),
            request: request.clone(), context: context.clone(),
            request_bytes: 0, context_bytes: 0, probe,
            root: scope.snapshot, catalog: None,
            remaining, expires: (now_ms() + self.config.ttl).min(remaining.deadline_unix_ms),
        };
        build.request_bytes = value_bytes(&build.request);
        build.context_bytes = value_bytes(&build.context);
        self.registered_graph_step(&self.token("registered-build"), build, &mut reservation, cancel, usage)
    }

    fn registered_graph_step(
        &self, token: &str, mut build: RegisteredBuild,
        reservation: &mut ProjectionReservation<'_>, cancel: &Cancellation, usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        cancel.check(build.remaining.deadline_unix_ms)?;
        let scope = self.renew_scope_for_work(&build.request["view"]["handle"],
            &build.context, build.remaining.deadline_unix_ms)?;
        if scope.snapshot.stamp != build.root.stamp
            || !classifier_eq(&build.request["view"]["classifier"], &scope.classifier)?
        {
            return Err(SnapshotError::new(Status::Changed, "prepared graph root generation changed"));
        }
        match self.catalog_access(CatalogInput {
            ids: build.request["thread_ids"].as_array().expect("validated registered ids"),
            roots: build.request["roots"].as_array().expect("validated registered roots"),
        }, &build.context, cancel, &mut build.remaining, usage, CatalogMode::Index(&mut build.probe))?
        {
            CatalogResolution::Indexed => {
                self.extend_projection_reservation(reservation, &build.context,
                    value_bytes(&build.request["limits"]))?;
                build.request.insert("limits", build.remaining.to_json());
                self.prepare_graph_ready(&build.request, &build.context, cancel, usage)
            }
            CatalogResolution::Ready { .. } => unreachable!("catalog indexing does not materialize paths"),
            CatalogResolution::Pending(slot) => {
                build.catalog = Some(slot);
                let mut state = self.lock_state();
                if state.registered_builds.len() + state.prepared_builds.len() >= self.lease_cap(&build.context)? {
                    return Err(SnapshotError::new(Status::LeaseLimit, "prepared build admission exhausted"));
                }
                build.expires = (now_ms() + self.config.ttl).min(build.remaining.deadline_unix_ms);
                let token = token.to_owned();
                let pledge = Delivery::cursor_pledge(&build.claimant, &token);
                let additional = charged_bytes(&token, &build) + state.registered_builds.growth_for(&token) + pledge;
                let covered = additional.min(reservation.bytes);
                self.admit_memory(&mut state, &build.context, additional - covered)?;
                state.transient_bytes -= covered;
                reservation.bytes -= covered;
                state.registered_builds.reserve_for(&token);
                state.registered_builds.insert(token.clone(), build);
                state.registered_builds.pledge(&token, pledge);
                Ok((Value::new_null(), Some(token), Some("prepared graph work incomplete".to_owned())))
            }
        }
    }
}

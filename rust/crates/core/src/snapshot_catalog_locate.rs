enum CatalogCachePublication {
    Uncertified,
    Complete { generation_version: u64 },
}

enum CatalogPrefixCertificate<'a> {
    Closed { generation_version: u64, through: &'a [usize] },
    Before { generation_version: u64, before: &'a [usize] },
    Complete { generation_version: u64 },
}

struct CatalogLocateQuery {
    ids: Vec<String>,
    wanted: HashSet<String>,
    found: HashSet<String>,
    scope: Vec<PathBuf>,
    pending: VecDeque<Value>,
    output: Vec<Value>,
    page_bytes: usize,
    emitted: usize,
    output_bytes: usize,
    finished: bool,
    exhausted: bool,
    choices: BinaryHeap<Reverse<CatalogChoice>>,
    choice_bytes: usize,
    choices_ready: bool,
    choice_version: u64,
    floor: Option<Vec<usize>>,
    validated_after: Option<Vec<usize>>,
    observation_started: bool,
    used_name_stream: bool,
    cache_bridge: bool,
    cache_bridge_depth: Option<usize>,
    seen_certified_through: Option<Vec<usize>>,
    next_observation_root: usize,
    frames: Vec<CatalogObservationFrame>,
    frame_bytes: usize,
    seen_directories: HashSet<SourceIdentity>,
    generation_seen: Option<u64>,
    catalog_owner: Option<usize>,
    retirement_hook: LedgerHook,
}

struct CatalogObservationFrame {
    generation: u64,
    lease: Arc<()>,
    identity: SourceIdentity,
    next_name: usize,
    raw_parent: PathBuf,
    order: Vec<usize>,
}

impl CatalogObservationFrame {
    fn owned_bytes(&self) -> usize {
        self.raw_parent.capacity() + self.order.capacity() * size_of::<usize>()
    }
}

struct CatalogQueryObservation {
    binding: CatalogBinding,
}

impl CatalogQueryObservation {
    fn classify(store: &NativeStore, path: &Path, parent: Option<&Path>, context: &Value) -> Result<(Self,bool),SnapshotError> {
        CatalogBinding::entry(store,path,parent,context,false).map(|(binding,shared,_)| (Self { binding },shared))
    }
}

enum CatalogObservationStep {
    Candidate { id: String, path: PathBuf, stamp: SourceStamp },
    Progress,
    CachedSpan,
    End,
}

struct CatalogLocatePage {
    sessions: Vec<Value>,
    complete: bool,
    exhausted: bool,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CatalogChoice {
    order: Vec<usize>,
    id: String,
    path: PathBuf,
}

impl CatalogLocateQuery {
    fn accounted_bytes(&self) -> usize {
        self.ids.capacity() * size_of::<String>()
            + self.ids.iter().map(String::capacity).sum::<usize>()
            + self.wanted.capacity() * size_of::<String>()
            + self.wanted.iter().map(String::capacity).sum::<usize>()
            + self.found.capacity() * size_of::<String>()
            + self.found.iter().map(String::capacity).sum::<usize>()
            + self.scope.capacity() * size_of::<PathBuf>()
            + self.scope.iter().map(PathBuf::capacity).sum::<usize>()
            + self.pending.capacity() * size_of::<Value>()
            + self.pending.iter().map(value_bytes).sum::<usize>()
            + self.output.capacity() * size_of::<Value>()
            + self.output.iter().map(value_bytes).sum::<usize>()
            + self.choices.capacity() * size_of::<Reverse<CatalogChoice>>() + self.choice_bytes
            + self.floor.as_ref().map_or(0,|order| order.capacity() * size_of::<usize>())
            + self.validated_after.as_ref().map_or(0,|order| order.capacity() * size_of::<usize>())
            + self.frames.capacity() * size_of::<CatalogObservationFrame>()
            + self.frame_bytes
            + self.seen_directories.capacity() * size_of::<SourceIdentity>()
            + self.seen_certified_through.as_ref().map_or(0,|order| order.capacity() * size_of::<usize>())
    }

    fn push_choice(&mut self, choice: CatalogChoice, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        if self.choices.len() == self.choices.capacity() {
            let next = (self.choices.capacity() * 2).max(4);
            budget.reserve((next - self.choices.capacity()) * size_of::<Reverse<CatalogChoice>>())?;
            self.choices.reserve_exact(next - self.choices.len());
            assert_eq!(self.choices.capacity(),next);
        }
        self.choice_bytes += choice.id.capacity() + choice.path.capacity() + choice.order.capacity() * size_of::<usize>();
        self.choices.push(Reverse(choice));
        Ok(())
    }

    fn pop_choice(&mut self) -> Option<CatalogChoice> {
        let Reverse(choice) = self.choices.pop()?;
        self.choice_bytes -= choice.id.capacity() + choice.path.capacity() + choice.order.capacity() * size_of::<usize>();
        Some(choice)
    }

    fn validation_position(&mut self, order: &[usize], edge: Option<usize>, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let length = order.len() + usize::from(edge.is_some());
        let previous = self.validated_after.as_ref().map_or(0,Vec::capacity);
        budget.reserve(length.saturating_sub(previous) * size_of::<usize>())?;
        let held = self.validated_after.get_or_insert_with(Vec::new);
        held.clear(); held.reserve_exact(length); held.extend_from_slice(order);
        if let Some(edge) = edge { held.push(edge); }
        Ok(())
    }

    fn position(&mut self, order: &[usize], edge: Option<usize>, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let length = order.len() + usize::from(edge.is_some());
        for slot in [&mut self.floor,&mut self.validated_after] {
            let previous = slot.as_ref().map_or(0,Vec::capacity);
            budget.reserve(length.saturating_sub(previous) * size_of::<usize>())?;
            let held = slot.get_or_insert_with(Vec::new);
            held.clear(); held.reserve_exact(length);
            held.extend_from_slice(order);
            if let Some(edge) = edge { held.push(edge); }
        }
        Ok(())
    }

    fn queue_unfound(&mut self, status: &str, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let unfound = || self.ids.iter().filter(|id| !self.found.contains(*id));
        let count = unfound().count();
        budget.reserve(deque_growth(&self.pending, count)
            + unfound().map(|id| located_item_bytes(id, status, None)).sum::<usize>())?;
        let predicted = deque_capacity_for(&self.pending, count);
        self.pending.reserve(count);
        for id in &self.ids {
            if !self.found.contains(id) {
                self.pending.push_back(json!({"session_id":id,"status":status,"path":null,"revision":null}));
            }
        }
        assert_eq!(self.pending.capacity(), predicted);
        Ok(())
    }

    fn queue_candidate(&mut self, store: &NativeStore, id: &str, path: PathBuf,
        stamp: SourceStamp, updates: &mut Vec<(String, PathBuf)>, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let revision = stamp.revision();
        budget.reserve(set_growth(&self.found, 1) + id.len()
            + deque_growth(&self.pending, 1)
            + located_item_bytes(id, "ok", Some((&path, &revision)))
            + vec_growth(updates, 1) + id.len() + path.capacity())?;
        self.found.insert(id.to_owned());
        self.pending.push_back(json!({"session_id":id,"status":"ok","path":path.to_string_lossy().as_ref(),"revision":revision}));
        #[cfg(test)]
        store.locate_items.fetch_add(1, Ordering::Relaxed);
        updates.push((id.to_owned(), path));
        Ok(())
    }

    fn drain(&mut self, store: &NativeStore, context: &Value, limits: &WorkLimits,
        cancel: &Cancellation, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        while let Some(mut item) = self.pending.pop_front() {
            cancel.check(limits.deadline_unix_ms)?;
            if item["status"].as_str() == Some("ok") {
                let Some(stamp) = store.location_candidate(Path::new(str_field(&item, "path")?), &self.scope, context)? else {
                    let id = str_field(&item, "session_id")?;
                    budget.reserve(self.found.len() * size_of::<String>())?;
                    let mut kept = Vec::with_capacity(self.found.len());
                    kept.extend(self.found.drain().filter(|found| found != id));
                    self.found.extend(kept);
                    continue;
                };
                #[cfg(test)]
                store.after_location_metadata("pending");
                let revision = stamp.revision();
                budget.reserve(revision.len())?;
                item.insert("revision", json!(revision));
            }
            let bytes = encoded_size(&item, MAX_DATA_BYTES)?;
            if self.output_bytes + bytes + 2 > limits.max_output_bytes.saturating_sub(64) {
                return Err(SnapshotError::new(Status::OutputLimit, "location output budget exhausted"));
            }
            if self.page_bytes + bytes + 2 > MAX_DATA_BYTES || self.output.len() >= 1024 {
                self.pending.push_front(item);
                break;
            }
            budget.reserve(vec_growth(&self.output, 1))?;
            self.output_bytes += bytes + 2;
            self.emitted += 1;
            self.page_bytes += bytes + 2;
            self.output.push(item);
        }
        Ok(())
    }

    fn page(&mut self) -> CatalogLocatePage {
        CatalogLocatePage { sessions:std::mem::take(&mut self.output),
            complete:self.finished && self.pending.is_empty(), exhausted:self.exhausted }
    }
}

impl SourceCatalog {
    fn choices(&self, wanted: &HashSet<String>, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<Vec<CatalogChoice>, SnapshotError> {
        let mut choices = Vec::new();
        for (index, root) in self.roots.iter().enumerate() {
            if !root.binding.regular || root.path.extension().is_none_or(|extension| extension != "jsonl") { continue; }
            let stem = root.path.file_stem().expect("catalog root stem").to_string_lossy();
            if let Some(id) = NativeStore::requested_location(&stem, wanted) {
                budget.reserve(vec_growth(&choices, 1) + size_of::<usize>() + id.len() + root.path.as_os_str().len())?;
                choices.push(CatalogChoice { order:vec![self.roots.len() - index - 1], id:id.to_owned(), path:root.path.clone() });
            }
        }
        for id in wanted {
            if let Some(matches) = self.candidates.get(id.as_str()) {
                for candidate in matches {
                    cancel.check(deadline)?;
                    let Some(route) = self.route_index.get(&candidate.directory) else { continue; };
                    let node = self.directories.get(&candidate.directory).expect("catalog directory");
                    let edge = &node.edges[candidate.edge];
                    let stem = Path::new(&edge.name).file_stem().expect("catalog source stem").to_string_lossy();
                    if NativeStore::requested_location(&stem, wanted) != Some(id.as_str()) { continue; }
                    budget.reserve(vec_growth(&choices, 1) + id.len() + route.path.as_os_str().len() + 1 + edge.name.len())?;
                    choices.push(CatalogChoice { order:Self::query_order(route, candidate.edge, budget)?, id:id.clone(), path:catalog_child_path(&route.path, &edge.name) });
                }
            }
        }
        choices.sort_unstable_by(|left, right| left.order.cmp(&right.order));
        Ok(choices)
    }

    fn locate_page(&mut self, store: &NativeStore, _ids: &[Value], context: &Value,
        cancel: &Cancellation, remaining: &mut WorkLimits, usage: &mut [u64;20],
        query: &mut CatalogLocateQuery, budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogLocatePage, SnapshotError> {
        if !query.observation_started && query.generation_seen.is_some_and(|version| version != self.generation_version) {
            if !query.frames.is_empty() {
                query.observation_started = true;
                query.cache_bridge = false;
                query.cache_bridge_depth = None;
            } else {
                query.validated_after = None;
                query.choices_ready = false;
            }
        }
        query.generation_seen = Some(self.generation_version);
        let mut updates = Vec::new();
        let mut parent_proof = None;
        let stop = usage[17].saturating_add(store.config.event_step.min(remaining.max_discovery_entries) as u64);
        loop {
            cancel.check(remaining.deadline_unix_ms)?;
            query.drain(store,context,remaining,cancel,budget)?;
            if !query.pending.is_empty() || query.finished || query.page_bytes >= MAX_DATA_BYTES || query.output.len() >= 1024 { break; }
            if query.found.len() == query.ids.len() { query.finished = true; break; }
            if query.found.len() >= remaining.max_sources {
                query.exhausted = true; query.finished = true;
                query.queue_unfound("incomplete",budget)?;
                continue;
            }
            if !query.observation_started {
                self.sync_routes(budget)?;
                if query.frames.iter().any(|frame| {
                    !self.directories.get(&frame.identity).is_some_and(|node| node.generation == frame.generation)
                        || !self.route_index.get(&frame.identity).is_some_and(|route| route.path == frame.raw_parent && route.order == frame.order)
                }) {
                    query.observation_started = true;
                    query.cache_bridge = false;
                    query.cache_bridge_depth = None;
                    continue;
                }
                if !query.choices_ready || query.choice_version != self.candidate_version {
                    let choices = self.choices(&query.wanted,cancel,remaining.deadline_unix_ms,budget)?;
                    query.choices.clear(); query.choice_bytes = 0;
                    for choice in choices {
                        if !query.found.contains(&choice.id) && query.floor.as_ref().is_none_or(|floor| choice.order > *floor) {
                            query.push_choice(choice,budget)?;
                        }
                    }
                    query.choices_ready = true;
                    query.choice_version = self.candidate_version;
                }
                if let Some(choice) = query.pop_choice() {
                    if query.found.contains(&choice.id) { continue; }
                    if self.frontier_closed(&choice.order) {
                        let terminal_root = choice.order.len() == 1
                            && self.roots[self.roots.len() - choice.order[0] - 1].binding.regular;
                        let bound = if terminal_root { std::ops::Bound::Excluded(&choice.order) }
                            else { std::ops::Bound::Included(&choice.order) };
                        if self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,bound,query,budget)?.is_none() {
                            query.push_choice(choice,budget)?;
                            continue;
                        }
                        let stamp = store.location_candidate(&choice.path,&query.scope,context)?;
                        if terminal_root && stamp.is_none()
                            && self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,
                                std::ops::Bound::Included(&choice.order),query,budget)?.is_none() {
                            query.push_choice(choice,budget)?;
                            continue;
                        }
                        self.observe_cached_prefix(query,&choice.order,budget)?;
                        query.position(&choice.order,None,budget)?;
                        if let Some(stamp) = stamp {
                            query.queue_candidate(store,&choice.id,choice.path,stamp,&mut updates,budget)?;
                        }
                        continue;
                    }
                    query.push_choice(choice,budget)?;
                }
                if remaining.max_discovery_entries == 0 {
                    query.exhausted = true; query.finished = true;
                    query.queue_unfound("incomplete",budget)?;
                    continue;
                }
                if self.complete {
                    if self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,std::ops::Bound::Unbounded,query,budget)?.is_none() { continue; }
                    if !self.complete { continue; }
                    self.publication = if query.used_name_stream { CatalogCachePublication::Uncertified }
                        else { CatalogCachePublication::Complete { generation_version:self.generation_version } };
                    query.finished = true;
                    query.queue_unfound("missing",budget)?;
                    continue;
                }
                query.observation_started = true;
            }
            if remaining.max_discovery_entries == 0 {
                query.exhausted = true; query.finished = true;
                query.queue_unfound("incomplete",budget)?;
                continue;
            }
            if usage[17] >= stop { break; }
            let before = usage[17];
            let observation = self.observe_next(store,context,cancel,remaining.deadline_unix_ms,query,usage,&mut parent_proof,budget)?;
            remaining.max_discovery_entries = remaining.max_discovery_entries.saturating_sub((usage[17] - before) as usize);
            match observation {
                CatalogObservationStep::Candidate { id,path,stamp } => query.queue_candidate(store,&id,path,stamp,&mut updates,budget)?,
                CatalogObservationStep::Progress => {},
                CatalogObservationStep::CachedSpan => {
                    query.observation_started = false;
                    query.choices_ready = false;
                    query.generation_seen = Some(self.generation_version);
                }
                CatalogObservationStep::End => {
                    query.finished = true;
                    query.queue_unfound("missing",budget)?;
                }
            }
        }
        cancel.check(remaining.deadline_unix_ms)?;
        store.remember_locations(updates.iter().map(|(id,path)| (id,path)),context);
        cancel.check(remaining.deadline_unix_ms)?;
        query.generation_seen = Some(self.generation_version);
        Ok(query.page())
    }
}

impl SourceCatalog {
    fn admit_observation_frame(&mut self, store: &NativeStore, path: PathBuf, order: Vec<usize>,
        binding: &CatalogBinding, query: &mut CatalogLocateQuery, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let stamp = binding.stamp.expect("observed directory stamp");
        if query.seen_directories.contains(&stamp.identity) { return Ok(()); }
        let canonical = binding.canonical.as_ref().expect("observed directory canonical");
        budget.reserve(set_growth(&query.seen_directories,1) + vec_growth(&query.frames,1)
            + path.capacity() + order.capacity() * size_of::<usize>()
            + self.route_index.growth_for(&stamp.identity) + path.capacity() + order.capacity() * size_of::<usize>()
            + Self::check_growth(order.len()))?;
        if self.directories.get(&stamp.identity).is_some_and(|node| !node.observed.unchanged(binding)
            || node.stream_identity.is_some_and(|opened| opened != stamp.identity)) {
            budget.reserve(binding.payload_bytes())?;
            self.invalidate_directory(stamp.identity,binding.clone(),budget)?;
        }
        if self.directories.get(&stamp.identity).is_some_and(|node|
            matches!(node.scan,CatalogScan::Issue { attempt,.. } if attempt != budget.attempt)) {
            self.restart_issue(stamp.identity,budget)?;
        }
        budget.reserve(canonical.capacity())?;
        self.schedule(canonical.clone(),stamp,budget)?;
        let route = CatalogRoute { path:path.clone(),order:order.clone() };
        self.install_route(stamp.identity,route);
        self.sync_routes(budget)?;
        let generation = self.directories.get(&stamp.identity).expect("observed directory").generation;
        self.open_generation(store,stamp.identity,generation,&path,budget)?;
        let node = self.directories.get(&stamp.identity).expect("opened observation generation");
        if let CatalogScan::Issue { status,reason,.. } = &node.scan { return Err(SnapshotError::new(*status,reason)); }
        query.seen_directories.insert(stamp.identity);
        let frame = CatalogObservationFrame { generation,lease:Arc::clone(&node.lease),identity:stamp.identity,
            next_name:0,raw_parent:path,order };
        query.frame_bytes += frame.owned_bytes();
        query.frames.push(frame);
        Ok(())
    }

    fn observe_cached_prefix(&self, query: &mut CatalogLocateQuery, bound: &[usize],
        budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let lower = query.seen_certified_through.as_ref().map_or(std::ops::Bound::Unbounded,|order| std::ops::Bound::Excluded(order.as_slice()));
        for (_,check) in self.checks.range::<[usize],_>((lower,std::ops::Bound::Excluded(bound))) {
            let identity = match check.kind {
                CatalogCheckKind::Root(index) if self.roots[index].binding.directory => self.roots[index].binding.stamp.map(|stamp| stamp.identity),
                CatalogCheckKind::Directory(identity) => Some(identity),
                _ => None,
            };
            if let Some(identity) = identity {
                let lease = Arc::clone(&self.directories.get(&identity).expect("certified prefix lease").lease);
                budget.reserve(set_growth(&query.seen_directories,1))?;
                query.seen_directories.insert(identity);
                drop(lease);
            }
        }
        budget.reserve(bound.len() * size_of::<usize>())?;
        query.seen_certified_through = Some(bound.to_vec());
        query.frames.clear();
        query.frame_bytes = 0;
        query.next_observation_root = self.roots.len() - bound[0] - 1;
        for length in 1..bound.len() {
            let order = &bound[..length];
            let identity = match self.checks.get(order).map(|check| check.kind) {
                Some(CatalogCheckKind::Root(index)) => self.roots[index].binding.stamp.expect("cached prefix root").identity,
                Some(CatalogCheckKind::Directory(identity)) => identity,
                Some(CatalogCheckKind::Edge(identity,index)) => self.directories.get(&identity).expect("cached prefix parent").edges[index].binding.stamp.expect("cached prefix directory").identity,
                _ => continue,
            };
            let node = self.directories.get(&identity).expect("cached prefix generation");
            let route = self.route_index.get(&identity).expect("cached prefix route");
            budget.reserve(vec_growth(&query.frames,1) + route.path.capacity() + order.len() * size_of::<usize>()
                + set_growth(&query.seen_directories,1))?;
            query.seen_directories.insert(identity);
            let frame = CatalogObservationFrame { generation:node.generation,lease:Arc::clone(&node.lease),identity,
                next_name:bound[length] + 1,raw_parent:route.path.clone(),order:order.to_vec() };
            query.frame_bytes += frame.owned_bytes();
            query.frames.push(frame);
        }
        Ok(())
    }

    fn publish_observed_name(&mut self, identity: SourceIdentity, generation: u64, name: OsString,
        binding: &CatalogBinding, evidence: CatalogNameEvidence, physical_parent: Option<&CatalogParentObservation>,
        raw_parent: &Path, order: &[usize], offset: usize, cancel: &Cancellation, deadline: u64, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let current = self.directories.get(&identity).is_some_and(|node| node.generation == generation);
        let fact = if current { self.prove_catalog_name(identity,generation,&name,evidence,binding,physical_parent,
            raw_parent,order,offset,cancel,deadline,budget)? } else { None };
        if let Some(proof) = fact {
            let (edge,symlink) = CatalogNameRecord::proven(name,proof);
            let pending = CatalogPendingEdge { directory:identity,edge,symlink };
            self.install_edge(&pending,budget)?;
            self.commit_edge(pending);
            return Ok(());
        }
        self.append_unproven_name(identity,generation,name,budget)
    }

    fn observe_next(&mut self, store: &NativeStore, context: &Value, cancel: &Cancellation, deadline: u64,
        query: &mut CatalogLocateQuery, usage: &mut [u64;20], parent_proof: &mut Option<(u64,Option<CatalogParentObservation>)>,
        budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogObservationStep,SnapshotError> {
        loop {
            cancel.check(deadline)?;
            #[cfg(test)]
            store.after_location_metadata("walk");
            if query.frames.is_empty() {
                if query.next_observation_root == 0 {
                    if self.next_root == 0 && self.pending.is_empty() {
                        self.complete = true;
                        self.publication = CatalogCachePublication::Uncertified;
                        self.decision_revision += 1;
                        self.epoch += 1;
                    }
                    return Ok(CatalogObservationStep::End);
                }
                let index = query.next_observation_root - 1;
                budget.scratch(LOCATE_USER_PATH_SLOTS + self.roots[index].path.capacity())?;
                let path = self.roots[index].path.clone();
                let binding = CatalogBinding::inspect(store,&path,context)?;
                if let Some((status,reason)) = &binding.issue {
                    if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
                }
                budget.reserve(binding.payload_bytes() + 2 * size_of::<usize>() + Self::check_growth(1))?;
                let order = vec![self.roots.len() - index - 1];
                self.update_root(index,binding.clone(),budget)?;
                self.next_root = self.next_root.min(index);
                self.install_check(order.clone(),CatalogCheck { kind:CatalogCheckKind::Root(index) });
                query.next_observation_root -= 1;
                if binding.directory {
                    self.admit_observation_frame(store,path,order,&binding,query,budget)?;
                    continue;
                }
                if let Some(candidate) = Self::observed_candidate(&path,&binding,query,budget)? { return Ok(candidate); }
                continue;
            }
            let frame = query.frames.last().expect("observation frame");
            let (identity,generation,offset) = (frame.identity,frame.generation,frame.next_name);
            let step = self.next_generation_name(identity,generation,offset,budget)?;
            let (name,evidence,appended) = match step {
                CatalogNameStep::End => {
                    let ended = query.frames.pop().expect("completed observation frame");
                    query.frame_bytes -= ended.owned_bytes();
                    if self.directories.get(&identity).is_some_and(|node| node.generation == generation) {
                        self.pending.retain(|pending| *pending != identity);
                    }
                    if query.cache_bridge_depth == Some(query.frames.len()) {
                        query.cache_bridge_depth = None;
                        while query.frames.last().is_some_and(|frame| {
                            let node = self.generation(frame.identity,frame.generation);
                            frame.next_name == node.edges.len() && matches!(node.scan,CatalogScan::Done)
                        }) {
                            let completed = query.frames.pop().expect("completed cached ancestor");
                            query.frame_bytes -= completed.owned_bytes();
                        }
                        if query.frames.is_empty() && query.next_observation_root == 0 {
                            query.cache_bridge = false;
                            continue;
                        }
                        let mut reusable = true;
                        for frame in &query.frames {
                            if !reusable { break; }
                            let node = self.generation(frame.identity,frame.generation);
                            reusable = self.directories.get(&frame.identity).is_some_and(|current| current.generation == frame.generation)
                                && CatalogBinding::parent_observation(store,&frame.raw_parent,&node.canonical,context).is_some()
                                && CatalogBinding::inspect(store,&frame.raw_parent,context).is_ok_and(|fresh| node.observed.unchanged(&fresh));
                        }
                        if reusable {
                            budget.reserve((ended.order.len() + 1) * size_of::<usize>())?;
                            let mut after = ended.order;
                            after.push(usize::MAX);
                            query.position(&after,None,budget)?;
                            if self.next_root == 0 && self.pending.is_empty() { self.complete = true; }
                            return Ok(CatalogObservationStep::CachedSpan);
                        }
                        query.cache_bridge = false;
                    }
                    if query.frames.is_empty() && query.next_observation_root != 0 {
                        budget.reserve(vec_growth(&ended.order,1))?;
                        let mut after = ended.order;
                        after.push(usize::MAX);
                        query.position(&after,None,budget)?;
                        query.cache_bridge = false;
                        query.cache_bridge_depth = None;
                        return Ok(CatalogObservationStep::CachedSpan);
                    }
                    continue;
                }
                CatalogNameStep::Issue { status,reason } => return Err(SnapshotError::new(status,reason)),
                CatalogNameStep::Name { name,evidence,appended } => (name,evidence,appended),
            };
            // Logical discovery is charged to this view even when another caller prefetched the name.
            usage[17] += 1;
            query.used_name_stream = true;
            #[cfg(test)]
            store.after_location_metadata("entry");
            let frame = query.frames.last().expect("observation frame");
            budget.scratch(LOCATE_USER_PATH_SLOTS + frame.raw_parent.capacity() + name.capacity())?;
            let path = catalog_child_path(&frame.raw_parent,&name);
            if parent_proof.as_ref().is_none_or(|(prior,_)| *prior != generation) {
                let physical = &self.generation(identity,generation).canonical;
                *parent_proof = Some((generation,CatalogBinding::parent_observation(store,&frame.raw_parent,physical,context)));
            }
            let proof = parent_proof.as_ref().and_then(|(_,proof)| proof.as_ref());
            let hint_parent = (context["authority"]["kind"].as_str() == Some("user"))
                .then(|| proof.map(|proof| proof.canonical.as_path())).flatten();
            let (observation,shared_parent) = CatalogQueryObservation::classify(store,&path,hint_parent,context)?;
            let binding = observation.binding;
            if !shared_parent && binding.issue.as_ref().is_none_or(|(status,_)| *status == Status::Missing) {
                let physical = &self.generation(identity,generation).canonical;
                *parent_proof = Some((generation,CatalogBinding::parent_observation(store,&frame.raw_parent,physical,context)));
            }
            let proof = parent_proof.as_ref().and_then(|(_,proof)| proof.as_ref());
            if appended {
                self.publish_observed_name(identity,generation,name,&binding,evidence,proof,&frame.raw_parent,&frame.order,offset,cancel,deadline,budget)?;
            }
            if !shared_parent { *parent_proof = None; }
            if let Some((status,reason)) = &binding.issue {
                if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
            }
            let frame = query.frames.last_mut().expect("observation frame");
            frame.next_name += 1;
            if binding.directory {
                budget.reserve((frame.order.len() + 1) * size_of::<usize>() + path.capacity())?;
                let mut order = frame.order.clone(); order.push(offset);
                self.admit_observation_frame(store,path,order,&binding,query,budget)?;
            } else if let Some(candidate) = Self::observed_candidate(&path,&binding,query,budget)? { return Ok(candidate); }
            return Ok(CatalogObservationStep::Progress);
        }
    }

    fn observed_candidate(path: &Path, binding: &CatalogBinding, query: &CatalogLocateQuery,
        budget: &mut CatalogBudget<'_, '_>) -> Result<Option<CatalogObservationStep>,SnapshotError> {
        if !binding.regular || path.extension().is_none_or(|extension| extension != "jsonl") { return Ok(None); }
        let stem = path.file_stem().expect("observed source stem").to_string_lossy();
        let Some(id) = NativeStore::requested_location(&stem,&query.wanted).filter(|id| !query.found.contains(*id)) else { return Ok(None); };
        budget.reserve(id.len() + path.as_os_str().len())?;
        Ok(Some(CatalogObservationStep::Candidate { id:id.to_owned(),path:path.to_owned(),stamp:binding.stamp.expect("observed source stamp") }))
    }
}

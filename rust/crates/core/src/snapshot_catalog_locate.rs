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
    frontier: Option<CatalogQueryFrontier>,
    validated_after: Option<Vec<usize>>,
    final_hold: bool,
    authority_epoch: u64,
    authority_checks: BTreeMap<Vec<usize>, PathBuf>,
    authority_check_bytes: usize,
    authority_view: Option<(u64, u64)>,
    capabilities: BTreeMap<Vec<usize>,CatalogCapability>,
    capability_bytes: usize,
    generation_seen: Option<u64>,
    catalog_owner: Option<usize>,
}

#[derive(Clone, Copy)]
enum CatalogQueryFrontier {
    Directory { identity: SourceIdentity, generation: u64, route_digest: [u8;32] },
    Root { index: usize, stamp: SourceStamp, canonical_digest: [u8;32] },
}

struct CatalogCapability {
    canonical_digest: [u8;32],
    route_digest: [u8;32],
    directory_generation: u64,
    stamp: SourceStamp,
    authority_epoch: u64,
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
            + self.capability_bytes + self.authority_check_bytes
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

    fn validation_position(&mut self, order: &[usize], budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let previous = self.validated_after.as_ref().map_or(0,Vec::capacity);
        budget.reserve(order.len().saturating_sub(previous) * size_of::<usize>())?;
        let held = self.validated_after.get_or_insert_with(Vec::new);
        held.clear(); held.reserve_exact(order.len()); held.extend_from_slice(order);
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

    fn remember_capability(&mut self, order: Vec<usize>, path: &Path, generation: u64, binding: &CatalogBinding,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let Some((canonical,stamp)) = binding.canonical.as_ref().zip(binding.stamp) else { return Ok(()); };
        let capability = CatalogCapability { canonical_digest:Sha256::digest(canonical.as_os_str().as_encoded_bytes()).into(),
            route_digest:Sha256::digest(path.as_os_str().as_encoded_bytes()).into(), directory_generation:generation,
            stamp, authority_epoch:self.authority_epoch };
        if let Some(previous) = self.capabilities.get_mut(order.as_slice()) { *previous = capability; }
        else {
            budget.reserve(size_of::<(Vec<usize>,CatalogCapability)>())?;
            self.capability_bytes += size_of::<(Vec<usize>,CatalogCapability)>() + order.capacity() * size_of::<usize>();
            self.capabilities.insert(order,capability);
        }
        Ok(())
    }

    fn reuses_capability(&self, order: &[usize], path: &Path, generation: u64, canonical: &Path, binding: &CatalogBinding) -> bool {
        self.capabilities.get(order).is_some_and(|capability|
            capability.authority_epoch < self.authority_epoch
                && capability.directory_generation == generation
                && capability.route_digest == <[u8;32]>::from(Sha256::digest(path.as_os_str().as_encoded_bytes()))
                && binding.stamp.is_some_and(|stamp| stamp.identity == capability.stamp.identity
                    && (!binding.directory || stamp == capability.stamp))
                && capability.canonical_digest == <[u8;32]>::from(Sha256::digest(canonical.as_os_str().as_encoded_bytes())))
    }

    fn rewind_generation(&mut self) {
        self.floor = None; self.frontier = None; self.validated_after = None;
        self.choices_ready = false; self.final_hold = false;
    }

    fn rewind_parent(&mut self, parent: &Vec<usize>, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let mut removed = 0;
        self.capabilities.retain(|order,_| {
            if order.starts_with(parent) {
                removed += size_of::<(Vec<usize>,CatalogCapability)>() + order.capacity() * size_of::<usize>();
                false
            } else { true }
        });
        self.capability_bytes -= removed;
        let previous = if let Some((order,_)) = self.capabilities.range::<Vec<usize>,_>(
            (std::ops::Bound::Unbounded,std::ops::Bound::Excluded(parent))).next_back() {
            budget.reserve(order.len() * size_of::<usize>())?;
            Some(order.clone())
        } else { None };
        self.rewind_generation();
        if let Some(previous) = previous { self.position(&previous,None,budget)?; }
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

    fn public_query_frontier(&self, query: &CatalogLocateQuery) -> Option<CatalogQueryFrontier> {
        let floor = query.floor.as_ref()?;
        for length in (1..=floor.len()).rev() {
            let Some(check) = self.checks.get(&floor[..length]) else { continue; };
            let identity = match check.kind {
                CatalogCheckKind::Root(index) => {
                    let root = &self.roots[index];
                    let stamp = root.binding.stamp?;
                    if !root.binding.directory {
                        return Some(CatalogQueryFrontier::Root { index, stamp,
                            canonical_digest:Sha256::digest(root.binding.canonical.as_ref()?.as_os_str().as_encoded_bytes()).into() });
                    }
                    stamp.identity
                }
                CatalogCheckKind::Directory(identity) => identity,
                CatalogCheckKind::Edge(identity,index) => {
                    let edge = &self.directories.get(&identity)?.edges[index];
                    if edge.binding.directory { edge.binding.stamp?.identity } else { identity }
                }
                CatalogCheckKind::ReadIssue(identity) => identity,
            };
            let route = self.route_index.get(&identity)?;
            let node = self.directories.get(&identity)?;
            return Some(CatalogQueryFrontier::Directory { identity, generation:node.generation,
                route_digest:Sha256::digest(route.path.as_os_str().as_encoded_bytes()).into() });
        }
        None
    }

    fn public_frontier_unchanged(&self, query: &CatalogLocateQuery) -> bool {
        let Some(floor) = query.floor.as_ref() else { return false; };
        match query.frontier {
            Some(CatalogQueryFrontier::Directory { identity, generation, route_digest }) =>
                self.directories.get(&identity).zip(self.route_index.get(&identity)).is_some_and(|(node,route)|
                    node.generation == generation && floor.starts_with(&route.order)
                        && route_digest == <[u8;32]>::from(Sha256::digest(route.path.as_os_str().as_encoded_bytes()))),
            Some(CatalogQueryFrontier::Root { index, stamp, canonical_digest }) => {
                let root = &self.roots[index];
                root.binding.stamp == Some(stamp) && root.binding.canonical.as_ref().is_some_and(|path|
                    canonical_digest == <[u8;32]>::from(Sha256::digest(path.as_os_str().as_encoded_bytes())))
            }
            None => false,
        }
    }

    fn public_authority_view(&self, query: &mut CatalogLocateQuery, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        if query.authority_epoch == 0 || query.authority_view == Some((query.authority_epoch, self.candidate_version)) {
            return Ok(());
        }
        query.authority_checks.clear();
        query.authority_check_bytes = 0;
        for (identity, route) in self.route_index.iter() {
            let node = self.directories.get(identity).expect("catalog authority directory");
            for (index, edge) in node.edges.iter().enumerate() {
                if edge.binding.directory || node.symlinks.binary_search(&index).is_ok() || edge.binding.issue.is_some() {
                    continue;
                }
                let order = Self::query_order(route, index, budget)?;
                if query.floor.as_ref().is_some_and(|floor| order <= *floor) {
                    continue;
                }
                let path_bytes = route.path.as_os_str().len() + 1 + edge.name.len();
                budget.reserve(size_of::<(Vec<usize>, PathBuf)>() + path_bytes)?;
                let path = catalog_child_path(&route.path, &edge.name);
                query.authority_check_bytes += size_of::<(Vec<usize>, PathBuf)>()
                    + order.capacity() * size_of::<usize>() + path.capacity();
                query.authority_checks.insert(order, path);
            }
        }
        query.authority_view = Some((query.authority_epoch, self.candidate_version));
        Ok(())
    }

    fn locate_page(&mut self, store: &NativeStore, _ids: &[Value], context: &Value,
        cancel: &Cancellation, remaining: &mut WorkLimits, usage: &mut [u64;20],
        query: &mut CatalogLocateQuery, budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogLocatePage, SnapshotError> {
        if query.generation_seen.is_some_and(|generation| generation != self.generation_version) {
            if self.public_frontier_unchanged(query) {
                query.validated_after = None;
                query.choices_ready = false;
                query.final_hold = false;
            } else {
                query.rewind_generation();
            }
            query.authority_view = None;
        }
        query.generation_seen = Some(self.generation_version);
        let mut updates = Vec::new();
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
            self.sync_routes(budget)?;
            self.public_authority_view(query, budget)?;
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
                    if !self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,Some(&choice.order),query,budget)? {
                        query.push_choice(choice,budget)?;
                        continue;
                    }
                    let stamp = store.location_candidate(&choice.path,&query.scope,context)?;
                    query.position(&choice.order,None,budget)?;
                    query.frontier = self.public_query_frontier(query);
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
                if !query.final_hold {
                    query.validated_after = None;
                    query.final_hold = true;
                }
                if !self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,None,query,budget)? { continue; }
                if !self.complete { continue; }
                query.finished = true;
                query.queue_unfound("missing",budget)?;
                continue;
            }
            if usage[17] >= stop { break; }
            let frontier = self.frontier_order(budget)?;
            if !self.hold_prefix(store,context,cancel,remaining.deadline_unix_ms,frontier.as_ref(),query,budget)? { continue; }
            let before = usage[17];
            let choice = self.advance_until(store,cancel,remaining.deadline_unix_ms,
                (stop - usage[17]) as usize,usage,budget,Some(query))?;
            remaining.max_discovery_entries = remaining.max_discovery_entries.saturating_sub((usage[17] - before) as usize);
            query.choice_version = self.candidate_version;
            query.generation_seen = Some(self.generation_version);
            if query.authority_epoch != 0 {
                query.authority_view = Some((query.authority_epoch, self.candidate_version));
            }
            if let Some(choice) = choice { query.push_choice(choice,budget)?; }
        }
        cancel.check(remaining.deadline_unix_ms)?;
        store.remember_locations(updates.iter().map(|(id,path)| (id,path)),context);
        cancel.check(remaining.deadline_unix_ms)?;
        Ok(query.page())
    }
}

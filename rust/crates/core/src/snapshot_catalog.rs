struct CatalogBudget<'a, 'b> {
    store: &'a NativeStore,
    context: &'b Value,
    reservation: &'b mut ProjectionReservation<'a>,
    attempt: u64,
    scratch_bytes: usize,
}

impl CatalogBudget<'_, '_> {
    fn reserve(&mut self, bytes: usize) -> Result<(), SnapshotError> {
        self.store.extend_projection_reservation(self.reservation, self.context, bytes)
    }

    fn scratch(&mut self, bytes: usize) -> Result<(),SnapshotError> {
        if bytes > self.scratch_bytes {
            self.reserve(bytes - self.scratch_bytes)?;
            self.scratch_bytes = bytes;
        }
        Ok(())
    }

}

#[derive(Clone)]
struct CatalogBinding {
    canonical: Option<PathBuf>,
    stamp: Option<SourceStamp>,
    directory: bool,
    regular: bool,
    issue: Option<(Status, String)>,
}

impl CatalogBinding {
    fn empty() -> Self {
        Self { canonical:None, stamp:None, directory:false, regular:false, issue:None }
    }

    fn canonical_path(store: &NativeStore, path: &Path) -> Result<PathBuf,SnapshotError> {
        #[cfg(test)]
        store.location_canonicalizations.fetch_add(1,Ordering::Relaxed);
        store.canonical_path(path).map_err(io_error)
    }

    fn metadata(canonical: PathBuf) -> Self {
        match std::fs::metadata(&canonical) {
            Ok(metadata) => Self { canonical:Some(canonical), stamp:Some(SourceStamp::of(&metadata)),
                directory:metadata.is_dir(), regular:metadata.is_file(), issue:None },
            Err(error) => {
                let error = io_error(error);
                Self { canonical:Some(canonical), issue:Some((error.status,error.reason)), ..Self::empty() }
            }
        }
    }

    fn inspect(store: &NativeStore, path: &Path, context: &Value) -> Result<Self,SnapshotError> {
        Self::inspect_entry(store, path, context, false)
    }

    fn inspect_entry(store: &NativeStore, path: &Path, context: &Value, defer_denial: bool) -> Result<Self,SnapshotError> {
        #[cfg(test)]
        if context["authority"]["kind"].as_str() == Some("restricted_roots") {
            store.authority_paths_materialized.fetch_add(1,Ordering::Relaxed);
        }
        let canonical = match Self::canonical_path(store,path) {
            Ok(canonical) => canonical,
            Err(error) => return Ok(Self { issue:Some((error.status,error.reason)), ..Self::empty() }),
        };
        if let Err(error) = store.authority(context,Some(&canonical)) {
            if !defer_denial { return Err(error); }
            // Index the ordered refusal, never inspect or traverse its target.
            return Ok(Self { canonical:Some(canonical), issue:Some((error.status,error.reason)), ..Self::empty() });
        }
        Ok(Self::metadata(canonical))
    }

    fn entry(store: &NativeStore, path: &Path, parent: Option<&Path>, context: &Value,
        defer_denial: bool) -> Result<(Self,bool),SnapshotError> {
        if context["authority"]["kind"].as_str() == Some("user") {
            if let Some(parent) = parent {
                let canonical = catalog_child_path(parent,path.file_name().expect("catalog entry name"));
                if let Ok(metadata) = std::fs::symlink_metadata(&canonical) {
                    #[cfg(test)]
                    store.after_location_metadata("canonical_prefilter");
                    if let Ok(raw) = std::fs::symlink_metadata(path) {
                        if metadata.is_file() && raw.is_file()
                            && SourceStamp::of(&metadata).identity == SourceStamp::of(&raw).identity {
                            #[cfg(test)]
                            store.after_location_metadata("prefilter");
                            return Ok((Self { canonical:Some(canonical), stamp:Some(SourceStamp::of(&metadata)),
                                directory:false, regular:true, issue:None },true));
                        }
                    }
                }
            }
        }
        #[cfg(test)]
        store.location_entry_canonicalizations.fetch_add(1,Ordering::Relaxed);
        Self::inspect_entry(store,path,context,defer_denial).map(|binding| (binding,false))
    }

    fn unchanged(&self, fresh: &Self) -> bool {
        self.canonical == fresh.canonical && self.directory == fresh.directory
            && self.regular == fresh.regular && self.issue == fresh.issue
            && match (self.stamp, fresh.stamp) {
                (Some(old), Some(new)) if !self.directory => old.identity == new.identity,
                (old, new) => old == new,
            }
    }

    fn payload_bytes(&self) -> usize {
        self.canonical.as_ref().map_or(0, PathBuf::capacity)
            + self.issue.as_ref().map_or(0, |(_, reason)| reason.capacity())
    }
}

struct CatalogPhysicalEntryFact(Option<CatalogBinding>);

impl std::ops::Deref for CatalogPhysicalEntryFact {
    type Target = CatalogBinding;
    fn deref(&self) -> &CatalogBinding {
        static UNOBSERVED: CatalogBinding = CatalogBinding { canonical:None,stamp:None,directory:false,regular:false,issue:None };
        self.0.as_ref().unwrap_or(&UNOBSERVED)
    }
}

struct CatalogNameRecord {
    name: OsString,
    binding: CatalogPhysicalEntryFact,
    issue: Option<(Status,String)>,
}

type CatalogEdge = CatalogNameRecord;

impl CatalogNameRecord {
    fn canonical(name: OsString, mut observation: CatalogBinding) -> Self {
        let issue = observation.issue.take();
        let binding = CatalogPhysicalEntryFact(observation.stamp.is_some().then_some(observation));
        Self { name,binding,issue }
    }

    fn unobserved(name: OsString) -> Self {
        Self { name,binding:CatalogPhysicalEntryFact(None),issue:None }
    }

    fn payload_bytes(&self) -> usize {
        self.name.capacity() + self.binding.payload_bytes() + self.issue.as_ref().map_or(0,|(_,reason)| reason.capacity())
    }

    fn unchanged(&self, fresh: &CatalogBinding) -> bool {
        if self.issue.is_some() {
            return self.issue == fresh.issue && fresh.canonical.is_none() && fresh.stamp.is_none();
        }
        self.binding.unchanged(fresh)
    }
}

enum CatalogScan {
    Queued,
    Open(OpenDirectory),
    Done,
    Issue { status: Status, reason: String, position: usize, attempt: u64 },
    Gone,
}

impl CatalogScan {
    fn owned_bytes(&self) -> usize {
        match self {
            Self::Open(reader) => reader.retained_bytes(),
            Self::Issue { reason, .. } => reason.capacity(),
            _ => 0,
        }
    }
}

struct CatalogDirectory {
    generation: u64,
    lease: Arc<()>,
    pending_name: Option<(OsString, bool)>,
    physical_complete: bool,
    first_unproven: Option<usize>,
    canonical: PathBuf,
    observed: CatalogBinding,
    edges: Vec<CatalogEdge>,
    directories: Vec<usize>,
    symlinks: Vec<usize>,
    issues: Vec<usize>,
    scan: CatalogScan,
    payload_bytes: usize,
}

impl CatalogDirectory {
    fn owned_bytes(&self) -> usize {
        arc_bytes::<()>() + self.pending_name.as_ref().map_or(0, |(name,_)| name.capacity())
            + self.canonical.capacity() + self.observed.payload_bytes() + self.payload_bytes
            + self.edges.capacity() * size_of::<CatalogEdge>()
            + (self.directories.capacity() + self.symlinks.capacity() + self.issues.capacity()) * size_of::<usize>()
            + self.scan.owned_bytes()
    }
}

enum CatalogNameStep {
    Name { name: OsString, symlink: bool, appended: bool },
    End,
    Issue { status: Status, reason: String },
}

struct CatalogRoot {
    path: PathBuf,
    binding: CatalogBinding,
}

#[derive(Clone, Copy)]
struct CatalogCandidate {
    directory: SourceIdentity,
    edge: usize,
}

struct CatalogPendingEdge {
    directory: SourceIdentity,
    edge: CatalogEdge,
    symlink: bool,
}

impl CatalogPendingEdge {
    fn owned_bytes(&self) -> usize {
        self.edge.payload_bytes()
    }
}

#[derive(Clone, Copy)]
enum CatalogCheckKind {
    Root(usize),
    Directory(SourceIdentity),
    Edge(SourceIdentity, usize),
    ReadIssue(SourceIdentity),
}

#[derive(Clone, Copy)]
struct CatalogCheck {
    kind: CatalogCheckKind,
}

struct SourceCatalog {
    roots: Vec<CatalogRoot>,
    next_root: usize,
    directories: Table<SourceIdentity, CatalogDirectory>,
    retired_generations: Table<u64, CatalogDirectory>,
    retirement_queue: Vec<u64>,
    retired_bytes: usize,
    publication: CatalogCachePublication,
    physical_append_work: u64,
    candidates: Table<String, Vec<CatalogCandidate>>,
    route_index: Table<SourceIdentity, CatalogRoute>,
    route_bytes: usize,
    routes_dirty: bool,
    checks: BTreeMap<Vec<usize>, CatalogCheck>,
    check_bytes: usize,
    retry_issues: Vec<SourceIdentity>,
    edge_issues: usize,
    issue_bound: Option<(SourceIdentity, usize)>,
    issues_dirty: bool,
    pending: Vec<SourceIdentity>,
    roots_bytes: usize,
    directory_bytes: usize,
    index_bytes: usize,
    incarnation: u64,
    decision_revision: u64,
    selection_revision: u64,
    epoch: u64,
    candidate_version: u64,
    generation_version: u64,
    directory_sequence: u64,
    complete: bool,
}

struct CatalogRoute {
    path: PathBuf,
    order: Vec<usize>,
}

impl CatalogRoute {
    fn owned_bytes(&self) -> usize {
        self.path.capacity() + self.order.capacity() * size_of::<usize>()
    }
}

struct CatalogReadPlan {
    scope: Vec<PathBuf>,
    issue: Option<(Vec<usize>, Status, String, bool)>,
}

fn catalog_child_path(parent: &Path, name: &OsStr) -> PathBuf {
    let mut path = PathBuf::with_capacity(parent.as_os_str().len() + 1 + name.len());
    path.push(parent);
    path.push(name);
    path
}

impl SourceCatalog {
    fn charge(&self) -> usize {
        size_of::<Self>() + self.roots_bytes
            + self.roots.capacity() * size_of::<CatalogRoot>()
            + self.directories.reserved_bytes() + self.candidates.reserved_bytes()
            + self.directory_bytes + self.index_bytes
            + self.retired_generations.reserved_bytes() + self.retired_bytes
            + self.retirement_queue.capacity() * size_of::<u64>()
            + self.route_index.reserved_bytes() + self.route_bytes
            + self.check_bytes
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
    }

    #[cfg(test)]
    fn audit_charge(&self) -> usize {
        size_of::<Self>()
            + self.roots.capacity() * size_of::<CatalogRoot>()
            + self.roots.iter().map(|root| root.path.capacity() + root.binding.payload_bytes()).sum::<usize>()
            + self.directories.reserved_bytes() + self.candidates.reserved_bytes()
            + self.directories.values().chain(self.retired_generations.values()).map(|node| {
                arc_mirror::<()>() + node.pending_name.as_ref().map_or(0, |(name,_)| name.capacity()) + node.canonical.capacity() + node.observed.payload_bytes()
                    + node.edges.capacity() * size_of::<CatalogEdge>()
                    + node.edges.iter().map(|edge| edge.payload_bytes()).sum::<usize>()
                    + (node.directories.capacity() + node.symlinks.capacity() + node.issues.capacity()) * size_of::<usize>()
                    + node.scan.owned_bytes()
            }).sum::<usize>()
            + self.retired_generations.reserved_bytes()
            + self.retirement_queue.capacity() * size_of::<u64>()
            + self.candidates.iter().map(|(key, matches)| key.capacity() + matches.capacity() * size_of::<CatalogCandidate>()).sum::<usize>()
            + self.route_index.reserved_bytes()
            + self.route_index.values().map(CatalogRoute::owned_bytes).sum::<usize>()
            + self.checks.keys().map(|order| size_of::<(Vec<usize>,CatalogCheck)>() + order.capacity() * size_of::<usize>()).sum::<usize>()
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
    }

    fn new(store: &NativeStore, roots: &[Value], _context: &Value, budget: &mut CatalogBudget<'_, '_>) -> Result<Self, SnapshotError> {
        budget.reserve(size_of::<Self>() + roots.len() * size_of::<CatalogRoot>()
            + roots.iter().map(|root| root.as_str().map_or(0,str::len)).sum::<usize>())?;
        let roots = roots.iter().map(|root| Ok(CatalogRoot {
            path:PathBuf::from(root.as_str().ok_or_else(|| invalid("invalid root"))?), binding:CatalogBinding::empty(),
        })).collect::<Result<Vec<_>, SnapshotError>>()?;
        let work = store.lock_state().ledger.shared.work().clone();
        let roots_bytes = roots.iter().map(|root| root.path.capacity()).sum();
        Ok(Self {
            next_root:roots.len(), roots, roots_bytes,
            directories:Table::new(work.clone()), retired_generations:Table::new(work.clone()), retirement_queue:Vec::new(), retired_bytes:0, publication:CatalogCachePublication::Uncertified, physical_append_work:0, candidates:Table::new(work.clone()),
            route_index:Table::new(work), route_bytes:0, routes_dirty:false, checks:BTreeMap::new(), check_bytes:0, retry_issues:Vec::new(), edge_issues:0, issue_bound:None, issues_dirty:false,
            pending:Vec::new(),
            directory_bytes:0, index_bytes:0, incarnation:store.sequence.fetch_add(1, Ordering::Relaxed) as u64, decision_revision:0, selection_revision:0, epoch:0, candidate_version:0, generation_version:0, directory_sequence:0, complete:false,
        })
    }

    fn generation(&self, identity: SourceIdentity, generation: u64) -> &CatalogDirectory {
        if let Some(node) = self.directories.get(&identity).filter(|node| node.generation == generation) { return node; }
        self.retired_generations.get(&generation).expect("leased directory generation")
    }

    fn generation_mut(&mut self, identity: SourceIdentity, generation: u64) -> &mut CatalogDirectory {
        if self.directories.get(&identity).is_some_and(|node| node.generation == generation) {
            return self.directories.get_mut(&identity).expect("current generation");
        }
        self.retired_generations.get_mut(&generation).expect("leased directory generation")
    }

    fn retire_directory(&mut self, identity: SourceIdentity, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let node = self.directories.get(&identity).expect("retiring directory");
        let pinned = Arc::strong_count(&node.lease) > 1;
        if pinned { budget.reserve(self.retired_generations.growth_for(&node.generation) + vec_growth(&self.retirement_queue,1))?; }
        let node = self.directories.remove(&identity).expect("retiring directory");
        self.directory_bytes -= node.owned_bytes();
        if pinned {
            self.retired_bytes += node.owned_bytes();
            self.retirement_queue.push(node.generation);
            self.retired_generations.insert(node.generation,node);
        }
        Ok(())
    }

    fn replace_generation(&mut self, identity: SourceIdentity, fresh: Option<CatalogBinding>, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        let old = self.directories.get(&identity).expect("replacing directory");
        budget.reserve(old.canonical.capacity() + old.observed.payload_bytes() + arc_bytes::<()>()
            + self.directories.growth_for(&identity))?;
        let canonical = old.canonical.clone();
        let observed = fresh.unwrap_or_else(|| old.observed.clone());
        let live = observed.directory && observed.stamp.is_some_and(|stamp| stamp.identity == identity);
        self.retire_directory(identity,budget)?;
        self.directory_sequence += 1;
        let node = CatalogDirectory { generation:self.directory_sequence, lease:Arc::new(()), pending_name:None,
            physical_complete:true, first_unproven:None, canonical, observed, edges:Vec::new(), directories:Vec::new(), symlinks:Vec::new(),
            issues:Vec::new(), scan:if live { CatalogScan::Queued } else { CatalogScan::Gone }, payload_bytes:0 };
        self.directory_bytes += node.owned_bytes();
        self.directories.insert(identity,node);
        self.publication = CatalogCachePublication::Uncertified;
        Ok(())
    }

    fn prune_retired(&mut self) {
        let mut index = 0;
        while index < self.retirement_queue.len() {
            let generation = self.retirement_queue[index];
            if Arc::strong_count(&self.retired_generations.get(&generation).expect("retirement queue").lease) != 1 {
                index += 1;
                continue;
            }
            let node = self.retired_generations.remove(&generation).expect("retired generation");
            self.retired_bytes -= node.owned_bytes();
            self.retirement_queue.swap_remove(index);
        }
    }

    fn update_generation_charge(&mut self, identity: SourceIdentity, generation: u64, before: usize) {
        let after = self.generation(identity,generation).owned_bytes();
        if self.directories.get(&identity).is_some_and(|node| node.generation == generation) {
            self.directory_bytes = self.directory_bytes - before + after;
        } else { self.retired_bytes = self.retired_bytes - before + after; }
    }

    fn open_generation(&mut self, store: &NativeStore, identity: SourceIdentity, generation: u64,
        path: &Path, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        if !matches!(self.generation(identity,generation).scan,CatalogScan::Queued) { return Ok(()); }
        store.authority(budget.context,Some(&self.generation(identity,generation).canonical))?;
        budget.reserve(READ_DIR_HANDLE_BYTES + path.as_os_str().len() + FILESYSTEM_PATH_BYTES)?;
        let before = self.generation(identity,generation).owned_bytes();
        let scan = match OpenDirectory::open(path) {
            Ok(reader) => CatalogScan::Open(reader),
            Err(error) => CatalogScan::Issue { status:error.status,reason:error.reason,position:0,attempt:budget.attempt },
        };
        self.generation_mut(identity,generation).scan = scan;
        self.update_generation_charge(identity,generation,before);
        Ok(())
    }

    fn next_generation_name(&mut self, identity: SourceIdentity, generation: u64, offset: usize,
        budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogNameStep,SnapshotError> {
        let node = self.generation(identity,generation);
        if let Some(edge) = node.edges.get(offset) {
            budget.scratch(LOCATE_USER_PATH_SLOTS + edge.name.capacity())?;
            return Ok(CatalogNameStep::Name { name:edge.name.clone(),symlink:node.symlinks.binary_search(&offset).is_ok(),appended:false });
        }
        assert_eq!(offset,node.edges.len(),"generation name offset");
        if let Some((name,symlink)) = &node.pending_name {
            budget.scratch(LOCATE_USER_PATH_SLOTS + name.capacity())?;
            return Ok(CatalogNameStep::Name { name:name.clone(),symlink:*symlink,appended:true });
        }
        if let CatalogScan::Issue { status,reason,.. } = &node.scan {
            budget.scratch(LOCATE_USER_PATH_SLOTS + reason.capacity())?;
            return Ok(CatalogNameStep::Issue { status:*status,reason:reason.clone() });
        }
        if matches!(node.scan,CatalogScan::Done | CatalogScan::Gone) { return Ok(CatalogNameStep::End); }
        budget.reserve(FILESYSTEM_PATH_BYTES + vec_growth(&node.edges,1))?;
        let before = node.owned_bytes();
        let node = self.generation_mut(identity,generation);
        let CatalogScan::Open(reader) = &mut node.scan else { unreachable!("opened generation"); };
        let appended = match reader.next() {
            Some(Ok(entry)) => {
                let symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
                node.pending_name = Some((entry.file_name(),symlink));
                true
            }
            Some(Err(error)) => {
                let error = io_error(error);
                node.scan = CatalogScan::Issue { status:error.status,reason:error.reason,position:offset,attempt:budget.attempt };
                false
            }
            None => { node.scan = CatalogScan::Done; false }
        };
        if appended { self.physical_append_work += 1; }
        self.update_generation_charge(identity,generation,before);
        self.next_generation_name(identity,generation,offset,budget)
    }

    fn next_is_root(&self) -> bool {
        if self.next_root == 0 { return false; }
        let order = self.roots.len() - self.next_root;
        self.pending.last().is_none_or(|identity|
            self.route_index.get(identity).expect("catalog frontier route").order[0] >= order)
    }

    fn observe_root(&mut self, store: &NativeStore, context: &Value,
        budget: &mut CatalogBudget<'_, '_>) -> Result<usize, SnapshotError> {
        let index = self.next_root - 1;
        let root = &self.roots[index];
        let fresh = CatalogBinding::inspect(store, &root.path, context)?;
        budget.reserve(fresh.payload_bytes())?;
        if fresh.directory {
            let canonical = fresh.canonical.as_ref().expect("catalog root directory");
            let stamp = fresh.stamp.expect("catalog root stamp");
            budget.reserve(canonical.as_os_str().len() + self.route_index.growth_for(&stamp.identity)
                + root.path.as_os_str().len() + size_of::<usize>() + Self::check_growth(1))?;
            let route = CatalogRoute { path:root.path.clone(), order:vec![self.roots.len() - index - 1] };
            self.schedule(canonical.clone(), stamp, budget)?;
            self.install_route(stamp.identity, route);
        }
        budget.reserve(Self::check_growth(1))?;
        let root = &mut self.roots[index];
        self.roots_bytes = self.roots_bytes - root.binding.payload_bytes() + fresh.payload_bytes();
        root.binding = fresh;
        self.decision_revision += 1;
        self.selection_revision += 1;
        self.next_root -= 1;
        self.candidate_version += 1;
        self.install_check(vec![self.roots.len() - index - 1], CatalogCheck { kind:CatalogCheckKind::Root(index) });
        Ok(index)
    }

    fn schedule(&mut self, canonical: PathBuf, stamp: SourceStamp, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let bytes = if let Some(node) = self.directories.get(&stamp.identity) {
            if matches!(node.scan, CatalogScan::Gone) {
                2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
            } else { usize::from(node.canonical != canonical) * canonical.as_os_str().len() }
        } else {
            self.directories.growth_for(&stamp.identity) + arc_bytes::<()>() + 2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
        };
        budget.reserve(bytes)?;
        self.install_directory(canonical, stamp, None);
        Ok(())
    }

    fn check_growth(order_len: usize) -> usize {
        size_of::<(Vec<usize>,CatalogCheck)>() + order_len * size_of::<usize>()
    }

    fn install_check(&mut self, order: Vec<usize>, check: CatalogCheck) {
        if let Some((previous,_)) = self.checks.remove_entry(order.as_slice()) {
            self.check_bytes -= Self::check_growth(previous.capacity());
        }
        self.check_bytes += Self::check_growth(order.capacity());
        self.checks.insert(order,check);
    }

    fn install_route(&mut self, identity: SourceIdentity, route: CatalogRoute) {
        if self.route_index.get(&identity).is_some_and(|old| old.order <= route.order) { return; }
        self.decision_revision += 1;
        if self.route_index.contains_key(&identity) { self.selection_revision += 1; }
        if !self.route_index.contains_key(&identity)
            && self.directories.get(&identity).is_some_and(|node| !node.edges.is_empty())
        { self.routes_dirty = true; }
        if let Some(old) = self.route_index.get(&identity) {
            self.route_bytes -= old.owned_bytes();
            if self.checks.get(old.order.as_slice()).is_some_and(|check| matches!(check.kind,CatalogCheckKind::Directory(_))) {
                let (order,_) = self.checks.remove_entry(old.order.as_slice()).expect("retired directory check");
                self.check_bytes -= Self::check_growth(order.capacity());
            }
            if self.directories.get(&identity).is_some_and(|node| !node.edges.is_empty()) {
                self.routes_dirty = true;
            }
        }
        let kind = if route.order.len() == 1 { CatalogCheckKind::Root(self.roots.len() - route.order[0] - 1) }
            else { CatalogCheckKind::Directory(identity) };
        self.install_check(route.order.clone(), CatalogCheck { kind });
        self.route_bytes += route.owned_bytes();
        self.route_index.insert(identity, route);
    }

    fn rebuild_checks(&mut self, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let mut checks = BTreeMap::new();
        for (index,root) in self.roots.iter().enumerate().skip(self.next_root) {
            budget.reserve(Self::check_growth(1))?;
            checks.insert(vec![self.roots.len() - index - 1], CatalogCheck { kind:CatalogCheckKind::Root(index) });
        }
        for (identity,route) in self.route_index.iter() {
            budget.reserve(Self::check_growth(route.order.len()))?;
            let kind = if route.order.len() == 1 { CatalogCheckKind::Root(self.roots.len() - route.order[0] - 1) }
                else { CatalogCheckKind::Directory(*identity) };
            checks.insert(route.order.clone(), CatalogCheck { kind });
        }
        for (identity,route) in self.route_index.iter() {
            let node = self.directories.get(identity).expect("catalog check node");
            for index in node.symlinks.iter().chain(&node.issues) {
                budget.reserve(Self::check_growth(route.order.len() + 1))?;
                let mut order = Vec::with_capacity(route.order.len() + 1);
                order.extend_from_slice(&route.order); order.push(*index);
                let edge = &node.edges[*index];
                checks.insert(order, CatalogCheck { kind:CatalogCheckKind::Edge(*identity,*index),
 });
            }
        }
        for (identity,route) in self.route_index.iter() {
            if let CatalogScan::Issue { position,.. } = self.directories.get(identity).expect("catalog issue node").scan {
                let order = Self::query_order(route,position,budget)?;
                budget.reserve(Self::check_growth(order.capacity()))?;
                checks.insert(order,CatalogCheck { kind:CatalogCheckKind::ReadIssue(*identity) });
            }
        }
        self.check_bytes = checks.keys().map(|order| Self::check_growth(order.capacity())).sum();
        self.checks = checks;
        Ok(())
    }

    fn sync_routes(&mut self, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        if !self.routes_dirty { return Ok(()); }
        let rebuilt = self.build_routes(budget)?;
        budget.reserve(self.route_index.growth(rebuilt.len().saturating_sub(self.route_index.len())))?;
        self.route_index.retain(|_, _| false);
        self.route_bytes = 0;
        self.route_index.reserve(rebuilt.len());
        for (identity, route) in rebuilt {
            self.route_bytes += route.owned_bytes();
            self.route_index.insert(identity, route);
        }
        let mut orphaned = Vec::new();
        for identity in &self.pending {
            if !self.route_index.contains_key(identity) {
                budget.reserve(vec_growth(&orphaned, 1))?;
                orphaned.push(*identity);
            }
        }
        for identity in orphaned {
            self.remove_directory_index(identity);
            self.replace_generation(identity, Some(CatalogBinding::empty()), budget)?;
        }
        let routes = &self.route_index;
        self.pending.retain(|identity| routes.contains_key(identity));
        self.sort_pending();
        let routes = &self.route_index;
        let nodes = &self.directories;
        let retry_count = routes.keys().filter(|identity|
            matches!(nodes.get(identity).expect("catalog route node").scan, CatalogScan::Issue { .. })).count();
        budget.reserve(vec_growth(&self.retry_issues, retry_count.saturating_sub(self.retry_issues.len())))?;
        self.retry_issues.clear();
        self.retry_issues.reserve(retry_count);
        self.retry_issues.extend(routes.keys().filter(|identity|
            matches!(nodes.get(identity).expect("catalog route node").scan, CatalogScan::Issue { .. })).copied());
        self.edge_issues = routes.keys().map(|identity| nodes.get(identity).expect("catalog route node").issues.len()).sum();
        self.rebuild_checks(budget)?;
        self.routes_dirty = false;
        self.rebuild_issue_bound();
        Ok(())
    }

    fn issue_precedes(&self, left: (SourceIdentity, usize), right: (SourceIdentity, usize)) -> bool {
        let left_route = self.route_index.get(&left.0).expect("catalog issue route");
        let right_route = self.route_index.get(&right.0).expect("catalog issue route");
        left_route.order.iter().copied().chain(std::iter::once(left.1))
            .cmp(right_route.order.iter().copied().chain(std::iter::once(right.1))).is_lt()
    }

    fn record_issue(&mut self, identity: SourceIdentity, position: usize) {
        if !self.route_index.contains_key(&identity) { return; }
        let candidate = (identity, position);
        if self.issue_bound.is_none_or(|old| self.issue_precedes(candidate, old)) {
            self.issue_bound = Some(candidate);
        }
    }

    fn rebuild_issue_bound(&mut self) {
        let mut bound = None;
        for identity in self.route_index.keys() {
            let node = self.directories.get(identity).expect("catalog issue directory");
            let scanned = match node.scan { CatalogScan::Issue { position, .. } => Some(position), _ => None };
            for position in node.issues.first().copied().into_iter().chain(scanned) {
                let candidate = (*identity, position);
                if bound.is_none_or(|old| self.issue_precedes(candidate, old)) { bound = Some(candidate); }
            }
        }
        self.issue_bound = bound;
        self.issues_dirty = false;
    }

    fn sort_pending(&mut self) {
        let routes = &self.route_index;
        let nodes = &self.directories;
        self.pending.sort_unstable_by(|left, right| {
            let left_route = routes.get(left).expect("catalog pending route");
            let right_route = routes.get(right).expect("catalog pending route");
            let left_next = matches!(nodes.get(left).expect("catalog pending node").scan, CatalogScan::Open(_))
                .then_some(nodes.get(left).unwrap().edges.len());
            let right_next = matches!(nodes.get(right).expect("catalog pending node").scan, CatalogScan::Open(_))
                .then_some(nodes.get(right).unwrap().edges.len());
            right_route.order.iter().copied().chain(right_next).cmp(left_route.order.iter().copied().chain(left_next))
        });
    }

    fn geometry_ready(&self, store: &NativeStore, ids: &[Value], chunk_size: usize,
        probe: &mut CatalogProbe) -> bool {
        if self.complete || self.routes_dirty || probe.invalid_chunk.is_some() { return true; }
        if self.issue_bound.is_some_and(|(identity, position)|
            self.route_index.get(&identity).is_some_and(|route| self.frontier_before_edge(route, position)))
        { return true; }
        if probe.epoch != self.epoch { probe.epoch = self.epoch; probe.next_id = 0; }
        let state = store.lock_state();
        while let Some(value) = ids.get(probe.next_id) {
            let id = value.as_str().expect("validated catalog probe id");
            let wanted = &probe.chunks[probe.next_id / chunk_size];
            let hinted = probe.use_hints && state.locations.get(id).is_some_and(|found|
                self.roots.iter().any(|root| found.path.starts_with(&root.path)));
            let rooted = self.roots.iter().enumerate().any(|(index, root)| {
                root.binding.regular && root.path.extension().is_some_and(|ext| ext == "jsonl")
                    && root.path.file_stem().is_some_and(|stem|
                        NativeStore::requested_location(&stem.to_string_lossy(), wanted) == Some(id))
                    && self.frontier_before_root(self.roots.len() - index - 1)
            });
            let indexed = self.candidates.get(id).is_some_and(|matches| matches.iter().any(|candidate| {
                let Some(route) = self.route_index.get(&candidate.directory) else { return false; };
                let edge = &self.directories.get(&candidate.directory).expect("catalog candidate directory").edges[candidate.edge];
                let stem = Path::new(&edge.name).file_stem().expect("catalog stem").to_string_lossy();
                NativeStore::requested_location(&stem, wanted) == Some(id)
                    && self.frontier_before_edge(route, candidate.edge)
            }));
            if !hinted && !rooted && !indexed { return false; }
            probe.next_id += 1;
        }
        true
    }

    fn frontier_before_root(&self, root: usize) -> bool {
        self.frontier_after(std::iter::once(root))
    }

    fn frontier_before_edge(&self, route: &CatalogRoute, edge: usize) -> bool {
        self.frontier_after(route.order.iter().copied().chain(std::iter::once(edge)))
    }

    fn frontier_after(&self, bound: impl Iterator<Item = usize> + Clone) -> bool {
        if self.next_root != 0 && std::iter::once(self.roots.len() - self.next_root).cmp(bound.clone()).is_le() { return false; }
        let Some(identity) = self.pending.last() else { return true; };
        let route = self.route_index.get(identity).expect("catalog frontier route");
        let node = self.directories.get(identity).expect("catalog frontier directory");
        let next = matches!(node.scan, CatalogScan::Open(_)).then_some(node.edges.len());
        route.order.iter().copied().chain(next).cmp(bound).is_gt()
    }

    fn index_names(stem: &str) -> impl Iterator<Item = &str> {
        std::iter::once(stem).chain(stem.match_indices('-').map(|(offset, _)| &stem[offset + 1..]))
    }

    fn install_directory(&mut self, canonical: PathBuf, stamp: SourceStamp, parent: Option<SourceIdentity>) {
        self.directory_sequence += 1;
        let generation = self.directory_sequence;
        self.decision_revision += 1;
        if let Some(node) = self.directories.get_mut(&stamp.identity) {
            if node.canonical != canonical { self.selection_revision += 1; }
            let before = node.owned_bytes();
            node.canonical = canonical;
            if matches!(node.scan, CatalogScan::Gone) {
                node.generation = generation;
                node.observed = CatalogBinding { canonical: Some(node.canonical.clone()), stamp: Some(stamp), directory: true, regular: false, issue: None };
                node.scan = CatalogScan::Queued;
                self.pending.push(stamp.identity);
                self.complete = false;
            } else if matches!(node.scan, CatalogScan::Queued | CatalogScan::Open(_)) {
                let ancestor = parent.is_some_and(|parent| {
                    self.route_index.get(&parent).zip(self.route_index.get(&stamp.identity))
                        .is_some_and(|(parent, child)| parent.order.starts_with(&child.order))
                });
                if !ancestor {
                    if let Some(position) = self.pending.iter().position(|identity| *identity == stamp.identity) {
                        self.pending.remove(position);
                    }
                    self.pending.push(stamp.identity);
                }
            }
            self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
            return;
        }
        let observed = CatalogBinding { canonical: Some(canonical.clone()), stamp: Some(stamp), directory: true, regular: false, issue: None };
        let node = CatalogDirectory {
            generation, lease:Arc::new(()), pending_name:None, physical_complete:true, first_unproven:None, canonical, observed, edges: Vec::new(), directories: Vec::new(), symlinks: Vec::new(), issues: Vec::new(),
            scan: CatalogScan::Queued, payload_bytes: 0,
        };
        self.directory_bytes += node.owned_bytes();
        self.directories.insert(stamp.identity, node);
        self.pending.push(stamp.identity);
        self.complete = false;
    }

    fn install_edge(&mut self, pending: &CatalogPendingEdge, budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        let node = self.directories.get(&pending.directory).expect("catalog directory");
        let edge = &pending.edge;
        let has_issue = edge.issue.as_ref().is_some_and(|(status, _)| *status != Status::Missing);
        let path = Path::new(&edge.name);
        let stem = path.file_stem().map(|stem| stem.to_string_lossy());
        let indexed = edge.binding.regular && path.extension().is_some_and(|extension| extension == "jsonl");
        let mut new_ids = 0;
        let mut index_growth = 0;
        if indexed {
            for id in Self::index_names(stem.as_ref().expect("catalog source stem")) {
                if let Some(candidates) = self.candidates.get(id) {
                    index_growth += vec_growth(candidates, 1);
                } else {
                    new_ids += 1;
                    index_growth += id.len() + vec_growth(&Vec::<CatalogCandidate>::new(), 1);
                }
            }
        }
        let directory_growth = if edge.binding.directory {
            let stamp = edge.binding.stamp.expect("catalog child stamp");
            let canonical = edge.binding.canonical.as_ref().expect("catalog child path");
            if let Some(child) = self.directories.get(&stamp.identity) {
                if matches!(child.scan, CatalogScan::Gone) {
                    2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
                } else { usize::from(child.canonical != *canonical) * canonical.as_os_str().len() }
            } else {
                self.directories.growth_for(&stamp.identity) + arc_bytes::<()>() + 2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
            }
        } else { 0 };
        let route_growth = if edge.binding.directory {
            let child = edge.binding.stamp.expect("catalog child stamp").identity;
            let route = self.route_index.get(&pending.directory).expect("catalog parent route");
            self.route_index.growth_for(&child) + route.path.as_os_str().len() + 1 + edge.name.len()
                + (route.order.len() + 1) * size_of::<usize>() + Self::check_growth(route.order.len() + 1)
        } else { 0 };
        let check_growth = if pending.symlink || has_issue {
            Self::check_growth(self.route_index.get(&pending.directory).expect("catalog parent route").order.len() + 1)
        } else { 0 };
        budget.reserve(check_growth + route_growth + vec_growth(&node.edges, 1) + pending.owned_bytes()
            + if pending.symlink { vec_growth(&node.symlinks, 1) } else { 0 }
            + if edge.binding.directory { vec_growth(&node.directories, 1) } else { 0 }
            + if has_issue { vec_growth(&node.issues, 1) } else { 0 }
            + self.candidates.growth(new_ids) + index_growth + directory_growth)?;
        Ok(())
    }

    fn commit_edge(&mut self, pending: CatalogPendingEdge) {
        self.decision_revision += 1;
        let directory = pending.directory;
        self.candidate_version += 1;
        if pending.symlink || pending.edge.issue.as_ref().is_some_and(|(status,_)| *status != Status::Missing) {
            let route = self.route_index.get(&directory).expect("catalog parent route");
            let mut order = Vec::with_capacity(route.order.len() + 1);
            order.extend_from_slice(&route.order);
            let edge = self.directories.get(&directory).expect("catalog directory").edges.len();
            order.push(edge);
            self.install_check(order, CatalogCheck { kind:CatalogCheckKind::Edge(directory,edge) });
        }
        if pending.edge.binding.directory {
            let parent = self.route_index.get(&directory).expect("catalog parent route");
            let mut order = Vec::with_capacity(parent.order.len() + 1);
            order.extend_from_slice(&parent.order);
            order.push(self.directories.get(&directory).expect("catalog directory").edges.len());
            let route = CatalogRoute { path: catalog_child_path(&parent.path, &pending.edge.name), order };
            let child = pending.edge.binding.stamp.expect("catalog child stamp").identity;
            self.install_route(child, route);
            self.install_directory(pending.edge.binding.canonical.as_ref().expect("catalog child path").clone(),
                pending.edge.binding.stamp.expect("catalog child stamp"), Some(directory));
        }
        if pending.symlink && pending.edge.binding.directory {
            let parent = self.route_index.get(&directory).expect("catalog parent route");
            let edge = self.directories.get(&directory).expect("catalog directory").edges.len();
            let mut order = Vec::with_capacity(parent.order.len() + 1);
            order.extend_from_slice(&parent.order); order.push(edge);
            self.install_check(order, CatalogCheck { kind:CatalogCheckKind::Edge(directory,edge),
 });
        }
        let node = self.directories.get_mut(&directory).expect("catalog directory");
        let before = node.owned_bytes();
        let edge_index = node.edges.len();
        if pending.symlink { node.symlinks.push(edge_index); }
        if pending.edge.binding.directory { node.directories.push(edge_index); }
        if pending.edge.issue.as_ref().is_some_and(|(status, _)| *status != Status::Missing) {
            self.edge_issues += 1;
            node.issues.push(edge_index);
        }
        node.payload_bytes += pending.edge.payload_bytes();
        node.pending_name = None;
        node.edges.push(pending.edge);
        self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
        let edge = &node.edges[edge_index];
        let path = Path::new(&edge.name);
        if edge.binding.regular && path.extension().is_some_and(|extension| extension == "jsonl") {
            let stem = path.file_stem().expect("catalog source stem").to_string_lossy();
            for id in Self::index_names(&stem) {
                if !self.candidates.contains_key(id) {
                    let id = id.to_owned();
                    self.index_bytes += id.capacity();
                    self.candidates.insert(id, Vec::new());
                }
                let candidates = self.candidates.get_mut(id).expect("catalog index");
                let before = candidates.capacity();
                candidates.push(CatalogCandidate { directory, edge: edge_index });
                self.index_bytes += (candidates.capacity() - before) * size_of::<CatalogCandidate>();
            }
        }
        if self.directories.get(&directory).expect("catalog directory").issues.last() == Some(&edge_index) {
            self.record_issue(directory, edge_index);
        }
    }

    fn restart_issue(&mut self, identity: SourceIdentity, budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        budget.reserve(vec_growth(&self.pending,1))?;
        self.retry_issues.retain(|candidate| *candidate != identity);
        self.remove_directory_index(identity);
        self.replace_generation(identity, None, budget)?;
        self.pending.push(identity);
        self.complete = false;
        self.routes_dirty = true;
        self.candidate_version += 1;
        self.generation_version += 1;
        self.sync_routes(budget)
    }

    fn advance(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        max_entries: usize, usage: &mut [u64;20], budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        let retries = self.retry_issues.iter().filter(|identity| self.directories.get(*identity).is_some_and(|node|
                matches!(node.scan,CatalogScan::Issue { attempt,.. } if attempt != budget.attempt))).count();
        budget.reserve(vec_growth(&self.pending,retries))?;
        self.pending.reserve(retries);
        let mut position = 0;
        let mut requeued = false;
        while position < self.retry_issues.len() {
            let identity = self.retry_issues[position];
            let issue = self.directories.get(&identity).and_then(|node| match node.scan {
                CatalogScan::Issue { attempt, .. } => Some(attempt), _ => None,
            });
            if issue.is_none() { self.retry_issues.swap_remove(position); continue; }
            if issue == Some(budget.attempt) { position += 1; continue; }
            self.retry_issues.swap_remove(position);
            self.remove_directory_index(identity);
            self.replace_generation(identity, None, budget)?;
            self.pending.push(identity);
            requeued = true;
            self.complete = false;
        }
        if requeued { self.generation_version += 1; self.sort_pending(); }
        if self.issues_dirty { self.rebuild_issue_bound(); }
        if self.complete { return Ok(()); }
        let mut examined = 0;
        loop {
            cancel.check(deadline)?;
            #[cfg(test)]
            store.after_location_metadata("walk");
            cancel.check(deadline)?;
            if self.next_is_root() {
                let context = budget.context;
                self.observe_root(store, context, budget)?;
                continue;
            }
            let Some(identity) = self.pending.last().copied() else {
                self.decision_revision += 1;
                self.epoch += 1;
                self.complete = true;
                return Ok(());
            };
            let generation = self.directories.get(&identity).expect("catalog directory").generation;
            if examined == max_entries { return Ok(()); }
            let canonical = &self.directories.get(&identity).expect("catalog directory").canonical;
            budget.scratch(LOCATE_USER_PATH_SLOTS + canonical.as_os_str().len())?;
            let canonical = canonical.clone();
            self.open_generation(store,identity,generation,&canonical,budget)?;
            let offset = self.directories.get(&identity).expect("catalog directory").edges.len();
            match self.next_generation_name(identity,generation,offset,budget)? {
                CatalogNameStep::Name { name,symlink,.. } => {
                    examined += 1;
                    usage[17] += 1;
                    #[cfg(test)]
                    store.after_location_metadata("entry");
                    let path = catalog_child_path(&canonical,&name);
                    let (binding,_) = CatalogBinding::entry(store,&path,Some(&canonical),budget.context,true)?;
                    let pending = CatalogPendingEdge { directory:identity,edge:CatalogNameRecord::canonical(name,binding),symlink };
                    self.install_edge(&pending,budget)?;
                    self.commit_edge(pending);
                }
                CatalogNameStep::End => { self.pending.pop(); }
                CatalogNameStep::Issue { .. } => {
                    budget.reserve(vec_growth(&self.retry_issues,1) + Self::check_growth(self.route_index.get(&identity).expect("issue route").order.len() + 1))?;
                    self.retry_issues.push(identity);
                    self.record_issue(identity,offset);
                    let order = Self::query_order(self.route_index.get(&identity).expect("issue route"),offset,budget)?;
                    self.install_check(order,CatalogCheck { kind:CatalogCheckKind::ReadIssue(identity) });
                    self.pending.pop();
                }
            }

        }
    }

    fn build_routes(&self, budget: &mut CatalogBudget<'_, '_>) -> Result<HashMap<SourceIdentity, CatalogRoute>, SnapshotError> {
        let mut routes = HashMap::new();
        let mut pending = Vec::new();
        for (index, root) in self.roots.iter().enumerate() {
            if root.binding.directory {
                budget.reserve(vec_growth(&pending, 1) + root.path.as_os_str().len() + size_of::<usize>())?;
                pending.push((root.binding.stamp.expect("catalog root stamp").identity,
                    CatalogRoute { path: root.path.clone(), order: vec![self.roots.len() - index - 1] }));
            }
        }
        while let Some((identity, route)) = pending.pop() {
            if routes.contains_key(&identity) { continue; }
            let Some(node) = self.directories.get(&identity) else { continue; };
            if matches!(node.scan, CatalogScan::Gone) { continue; }
            for index in node.directories.iter().rev() {
                let edge = &node.edges[*index];
                budget.reserve(vec_growth(&pending, 1)
                    + route.path.as_os_str().len() + 1 + edge.name.len()
                    + (route.order.len() + 1) * size_of::<usize>())?;
                let mut order = Vec::with_capacity(route.order.len() + 1);
                order.extend_from_slice(&route.order);
                order.push(*index);
                pending.push((edge.binding.stamp.expect("catalog child stamp").identity,
                    CatalogRoute { path: catalog_child_path(&route.path, &edge.name), order }));
            }
            budget.reserve(map_growth(&routes, 1))?;
            routes.insert(identity, route);
        }
        Ok(routes)
    }

    fn remove_directory_index(&mut self, identity: SourceIdentity) {
        self.decision_revision += 1;
        self.selection_revision += 1;
        let node = self.directories.get(&identity).expect("catalog directory");
        self.issues_dirty |= self.issue_bound.is_some_and(|bound| bound.0 == identity);
        if self.route_index.contains_key(&identity) { self.edge_issues -= node.issues.len(); }
        for edge in &node.edges {
            let path = Path::new(&edge.name);
            if !edge.binding.regular || path.extension().is_none_or(|extension| extension != "jsonl") { continue; }
            let stem = path.file_stem().expect("catalog source stem").to_string_lossy();
            for id in Self::index_names(&stem) {
                if let Some(candidates) = self.candidates.get_mut(id) {
                    candidates.retain(|candidate| candidate.directory != identity);
                    if candidates.is_empty() {
                        let (key, candidates) = self.candidates.remove_entry(id).expect("catalog index");
                        self.index_bytes -= key.capacity() + candidates.capacity() * size_of::<CatalogCandidate>();
                    }
                }
            }
        }
    }

    fn invalidate_directory(&mut self, identity: SourceIdentity, fresh: CatalogBinding,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.reserve(vec_growth(&self.pending, 1))?;
        self.remove_directory_index(identity);
        let same_directory = fresh.directory && fresh.stamp.is_some_and(|stamp| stamp.identity == identity);
        self.replace_generation(identity, Some(fresh), budget)?;
        self.pending.retain(|pending| *pending != identity);
        if same_directory { self.pending.push(identity); }
        self.complete = false;
        self.routes_dirty = true;
        self.epoch += 1;
        self.candidate_version += 1;
        self.generation_version += 1;
        self.publication = CatalogCachePublication::Uncertified;
        Ok(())
    }

    fn check_roots(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        for index in 0..self.roots.len() {
            cancel.check(deadline)?;
            let root = &self.roots[index];
            let fresh = CatalogBinding::inspect(store, &root.path, budget.context)?;
            if let Some((status, reason)) = &fresh.issue {
                self.selection_revision += 1;
                self.decision_revision += 1;
                self.complete = false;
                self.routes_dirty = true;
                self.epoch += 1;
                return Err(SnapshotError::new(*status, reason));
            }
            if root.binding.unchanged(&fresh) { continue; }
            self.update_root(index, fresh, budget)?;
        }
        // This registered/full-generation path has observed every declared root.
        self.next_root = 0;
        Ok(())
    }

    fn check_frontier(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        self.check_roots(store, cancel, deadline, budget)?;
        self.sync_routes(budget)?;
        let Some(identity) = self.pending.last().copied() else { return Ok(()); };
        let node = self.directories.get(&identity).expect("catalog frontier directory");
        let fresh = match CatalogBinding::inspect(store, &node.canonical, budget.context) {
            Ok(fresh) => fresh,
            Err(error) if error.status == Status::PermissionDenied => return Ok(()),
            Err(error) => return Err(error),
        };
        if !node.observed.unchanged(&fresh) || !node.physical_complete { self.invalidate_directory(identity, fresh, budget)?; }
        Ok(())
    }

    fn refresh(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        self.check_roots(store, cancel, deadline, budget)?;
        let mut changed = Vec::new();
        for (identity, node) in self.directories.iter() {
            cancel.check(deadline)?;
            let fresh = match CatalogBinding::inspect(store, &node.canonical, budget.context) {
                Ok(fresh) => fresh,
                Err(error) if error.status == Status::PermissionDenied => continue,
                Err(error) => return Err(error),
            };
            let mut dirty = !node.observed.unchanged(&fresh) || !node.physical_complete;
            for index in &node.symlinks {
                cancel.check(deadline)?;
                let edge = &node.edges[*index];
                let path = catalog_child_path(&node.canonical, &edge.name);
                match CatalogBinding::inspect(store, &path, budget.context) {
                    Ok(fresh) => dirty |= !edge.unchanged(&fresh),
                    Err(error) if error.status == Status::PermissionDenied => {},
                    Err(error) => return Err(error),
                }
            }
            if dirty {
                budget.reserve(vec_growth(&changed, 1) + fresh.payload_bytes())?;
                changed.push((*identity, fresh));
            }
        }
        for (identity, fresh) in changed { self.invalidate_directory(identity, fresh, budget)?; }
        self.sync_routes(budget)?;
        if self.next_root == 0 && self.pending.is_empty() {
            let reachable = &self.route_index;
            let mut removed = Vec::new();
            for identity in self.directories.keys() {
                if !reachable.contains_key(identity) {
                    budget.reserve(vec_growth(&removed, 1))?;
                    removed.push(*identity);
                }
            }
            for identity in removed {
                self.remove_directory_index(identity);
                self.retire_directory(identity, budget)?;
            }
        }
        Ok(())
    }

    fn frontier_order(&self, budget: &mut CatalogBudget<'_, '_>) -> Result<Option<Vec<usize>>,SnapshotError> {
        if self.next_is_root() {
            budget.reserve(size_of::<usize>())?;
            return Ok(Some(vec![self.roots.len() - self.next_root]));
        }
        let Some(identity) = self.pending.last() else { return Ok(None); };
        let route = self.route_index.get(identity).expect("catalog frontier route");
        let node = self.directories.get(identity).expect("catalog frontier directory");
        Ok(Some(Self::query_order(route,node.edges.len(),budget)?))
    }

    fn update_root(&mut self, index: usize, fresh: CatalogBinding,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(),SnapshotError> {
        budget.reserve(fresh.payload_bytes())?;
        if fresh.directory {
            let canonical = fresh.canonical.as_ref().expect("catalog root directory");
            let stamp = fresh.stamp.expect("catalog root stamp");
            let changed = self.directories.get(&stamp.identity).is_some_and(|node| !node.observed.unchanged(&fresh));
            if changed {
                budget.reserve(fresh.payload_bytes())?;
                self.invalidate_directory(stamp.identity,fresh.clone(),budget)?;
            }
            budget.reserve(canonical.as_os_str().len())?;
            self.schedule(canonical.clone(),stamp,budget)?;
        }
        let root = &mut self.roots[index];
        self.roots_bytes = self.roots_bytes - root.binding.payload_bytes() + fresh.payload_bytes();
        root.binding = fresh;
        self.decision_revision += 1;
        self.selection_revision += 1;
        self.routes_dirty = true;
        self.complete = false;
        self.epoch += 1;
        self.candidate_version += 1;
        self.generation_version += 1;
        Ok(())
    }

    fn hold_prefix<'c>(&mut self, store: &NativeStore, context: &Value, cancel: &Cancellation,
        deadline: u64, bound: Option<&'c Vec<usize>>, query: &mut CatalogLocateQuery,
        budget: &mut CatalogBudget<'_, '_>) -> Result<Option<CatalogPrefixCertificate<'c>>,SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        loop {
            cancel.check(deadline)?;
            let lower = query.validated_after.as_ref().map_or(std::ops::Bound::Unbounded,std::ops::Bound::Excluded);
            let upper = bound.map_or(std::ops::Bound::Unbounded,std::ops::Bound::Included);
            let Some((order,check)) = self.checks.range::<Vec<usize>,_>((lower,upper)).next() else {
                return Ok(Some(match bound {
                    Some(through) => CatalogPrefixCertificate::Closed { generation_version:self.generation_version,through },
                    None => CatalogPrefixCertificate::Complete { generation_version:self.generation_version },
                }));
            };
            let check = *check;
            query.validation_position(order,None,budget)?;
            if let CatalogCheckKind::ReadIssue(identity) = check.kind {
                self.restart_issue(identity,budget)?;
                query.validated_after = None;
                return Ok(None);
            }
            let path = match check.kind {
                CatalogCheckKind::Root(index) => self.roots[index].path.clone(),
                CatalogCheckKind::Directory(identity) => self.route_index.get(&identity).expect("catalog check route").path.clone(),
                CatalogCheckKind::Edge(identity,index) => catalog_child_path(&self.route_index.get(&identity).expect("catalog parent route").path,
                    &self.directories.get(&identity).expect("catalog parent").edges[index].name),
                CatalogCheckKind::ReadIssue(_) => unreachable!(),
            };
            let fresh = CatalogBinding::inspect(store,&path,context)?;
            if let Some((status,reason)) = &fresh.issue {
                if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
            }
            let old = match check.kind {
                CatalogCheckKind::Root(index) => &self.roots[index].binding,
                CatalogCheckKind::Directory(identity) => &self.directories.get(&identity).expect("catalog check directory").observed,
                CatalogCheckKind::Edge(identity,index) => &self.directories.get(&identity).expect("catalog parent").edges[index].binding,
                CatalogCheckKind::ReadIssue(_) => unreachable!(),
            };
            let uncertain_identity = match check.kind {
                CatalogCheckKind::Root(index) => self.roots[index].binding.stamp.filter(|_| self.roots[index].binding.directory).map(|stamp| stamp.identity),
                CatalogCheckKind::Directory(identity) => Some(identity),
                _ => None,
            };
            let uncertain = uncertain_identity.is_some_and(|identity| {
                let node = self.directories.get(&identity).expect("prefix directory");
                node.first_unproven.is_some_and(|offset| bound.is_none_or(|bound| {
                    let route = self.route_index.get(&identity).expect("prefix route");
                    route.order.iter().copied().chain(std::iter::once(offset)).cmp(bound.iter().copied()).is_le()
                }))
            });
            let unchanged = match check.kind {
                CatalogCheckKind::Edge(identity,index) => self.directories.get(&identity).expect("prefix parent").edges[index].unchanged(&fresh),
                _ => old.unchanged(&fresh),
            };
            if unchanged && !uncertain {
                if fresh.directory {
                    budget.reserve(set_growth(&query.seen_directories,1))?;
                    query.seen_directories.insert(fresh.stamp.expect("certified prefix directory").identity);
                }
                continue;
            }
            match check.kind {
                CatalogCheckKind::Root(index) => {
                    if uncertain && old.unchanged(&fresh) {
                        self.invalidate_directory(fresh.stamp.expect("directory stamp").identity,fresh,budget)?;
                    } else { self.update_root(index,fresh,budget)?; }
                }
                CatalogCheckKind::Directory(identity) => self.invalidate_directory(identity,fresh,budget)?,
                CatalogCheckKind::Edge(identity,_) => {
                    let observed = &self.directories.get(&identity).expect("catalog parent").observed;
                    budget.reserve(observed.payload_bytes())?;
                    self.invalidate_directory(identity,observed.clone(),budget)?;
                }
                CatalogCheckKind::ReadIssue(_) => unreachable!(),
            }
            self.sync_routes(budget)?;
            query.validated_after = None;
            query.choices_ready = false;
            return Ok(None);
        }
    }

    fn query_order(route: &CatalogRoute, edge: usize, budget: &mut CatalogBudget<'_, '_>) -> Result<Vec<usize>, SnapshotError> {
        budget.reserve((route.order.len() + 1) * size_of::<usize>())?;
        let mut order = Vec::with_capacity(route.order.len() + 1);
        order.extend_from_slice(&route.order);
        order.push(edge);
        Ok(order)
    }

    fn frontier_closed(&self, bound: &[usize]) -> bool {
        self.frontier_after(bound.iter().copied())
    }

    fn read_plan(&self, store: &NativeStore, context: &Value, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogReadPlan, SnapshotError> {
        budget.reserve(self.roots.len() * size_of::<PathBuf>()
            + self.roots.iter().map(|root| root.path.as_os_str().len()).sum::<usize>())?;
        let scope = self.roots.iter().map(|root| root.path.clone()).collect();
        let mut issue: Option<(Vec<usize>,Status,String,bool)> = None;
        for (index,root) in self.roots.iter().enumerate() {
            let root_order = self.roots.len() - index - 1;
            let error = root.binding.issue.as_ref().filter(|(status,_)| *status != Status::Missing)
                .map(|(status,reason)| SnapshotError::new(*status,reason))
                .or_else(|| root.binding.canonical.as_ref().and_then(|path| store.authority(context,Some(path)).err()));
            if let Some(error) = error {
                if issue.as_ref().is_none_or(|(old,_,_,_)| std::iter::once(root_order).cmp(old.iter().copied()).is_lt()) {
                    budget.reserve(size_of::<usize>() + error.reason.capacity())?;
                    issue = Some((vec![root_order],error.status,error.reason,false));
                }
            }
        }
        for (identity, route) in self.route_index.iter() {
            cancel.check(deadline)?;
            let node = self.directories.get(identity).expect("catalog directory");
            if let Err(error) = store.authority(context,Some(&node.canonical)) {
                if issue.as_ref().is_none_or(|(old,_,_,_)| route.order < *old) {
                    budget.reserve(route.order.len() * size_of::<usize>() + error.reason.capacity())?;
                    issue = Some((route.order.clone(),error.status,error.reason,false));
                }
            }
            for index in &node.symlinks {
                let edge = &node.edges[*index];
                if let Some(error) = edge.binding.canonical.as_ref().and_then(|path| store.authority(context,Some(path)).err()) {
                    let order = Self::query_order(route,*index,budget)?;
                    if issue.as_ref().is_none_or(|(old,_,_,_)| order < *old) {
                        budget.reserve(error.reason.capacity())?;
                        issue = Some((order,error.status,error.reason,false));
                    }
                }
            }
            if let CatalogScan::Issue { status, reason, position, attempt } = &node.scan {
                let order = Self::query_order(route, *position, budget)?;
                if issue.as_ref().is_none_or(|(old, _, _, _)| &order < old) {
                    budget.reserve(reason.len())?;
                    issue = Some((order, *status, reason.clone(), *attempt != budget.attempt));
                }
            }
            for index in &node.issues {
                let edge = &node.edges[*index];
                if let Some((status, reason)) = &edge.issue {
                    let order = Self::query_order(route, *index, budget)?;
                    if issue.as_ref().is_none_or(|(old, _, _, _)| &order < old) {
                        budget.reserve(reason.len())?;
                        issue = Some((order, *status, reason.clone(), false));
                    }
                }
            }
        }
        Ok(CatalogReadPlan { scope, issue })
    }

    fn cached_paths(&self, store: &NativeStore, ids: &[Value], budget: &mut CatalogBudget<'_, '_>) -> Result<HashMap<String, PathBuf>, SnapshotError> {
        let mut state = store.lock_state();
        let entries = || ids.iter().filter_map(|id| {
            let id = id.as_str().expect("validated session id");
            let path = &state.locations.get(id)?.path;
            self.roots.iter().any(|root| path.starts_with(&root.path)).then_some((id, path))
        });
        let count = entries().count();
        let payload = entries().map(|(id, path)| id.len() + path.as_os_str().len()).sum::<usize>();
        budget.store.extend_projection_reservation_in(&mut state, budget.reservation, budget.context,
            map_growth(&HashMap::<String, PathBuf>::new(), count) + payload)?;
        let mut cached = HashMap::with_capacity(count);
        for id in ids {
            let id = id.as_str().expect("validated session id");
            if let Some(found) = state.locations.get(id) {
                if self.roots.iter().any(|root| found.path.starts_with(&root.path)) {
                    cached.insert(id.to_owned(), found.path.clone());
                }
            }
        }
        Ok(cached)
    }

    fn try_locate(&self, store: &NativeStore, ids: &[Value], context: &Value, cancel: &Cancellation,
        deadline: u64, plan: &CatalogReadPlan, budget: &mut CatalogBudget<'_, '_>)
        -> Result<Option<HashMap<String, PathBuf>>, SnapshotError> {
        budget.reserve(LOCATE_USER_PATH_SLOTS + set_growth(&HashSet::<String>::new(), ids.len())
            + ids.iter().map(|id| id.as_str().expect("validated session id").len()).sum::<usize>()
            + self.roots.len() * size_of::<PathBuf>() + self.roots.iter().map(|root| root.path.as_os_str().len()).sum::<usize>())?;
        let mut wanted = HashSet::with_capacity(ids.len());
        wanted.extend(ids.iter().map(|id| id.as_str().expect("validated session id").to_owned()));
        let scope = &plan.scope;
        let cached = self.cached_paths(store, ids, budget)?;
        let choices = self.choices(&wanted, cancel, deadline, budget)?;
        let issue = &plan.issue;
        budget.reserve(set_growth(&HashSet::<&str>::new(), ids.len()))?;
        let mut possible: HashSet<&str> = HashSet::with_capacity(ids.len());
        possible.extend(cached.keys().map(String::as_str));
        let mut feasible = possible.len() == wanted.len();
        for choice in &choices {
            let order = &choice.order;
            if feasible { break; }
            if let Some((error_order, _, _, _)) = &issue {
                if error_order <= order && self.frontier_closed(error_order) { feasible = true; break; }
            }
            if !self.frontier_closed(order) { break; }
            possible.insert(choice.id.as_str());
            feasible = possible.len() == wanted.len();
        }
        feasible |= self.complete || issue.as_ref().is_some_and(|(order, _, _, _)| self.frontier_closed(order));
        if !feasible { return Ok(None); }
        let mut selected = HashMap::new();
        for value in ids {
            cancel.check(deadline)?;
            let id = value.as_str().expect("validated session id");
            if let Some(path) = cached.get(id) {
                if store.location_candidate(path, &scope, context)?.is_some() {
                    budget.reserve(map_growth(&selected, 1) + id.len() + path.as_os_str().len())?;
                    selected.insert(id.to_owned(), path.clone());
                }
            }
        }
        if selected.len() == wanted.len() { return Ok(Some(selected)); }
        for choice in choices {
            let CatalogChoice { order, id, path } = choice;
            cancel.check(deadline)?;
            if let Some((error_order, status, reason, stale)) = &issue {
                if error_order <= &order {
                    return if !*stale && self.frontier_closed(error_order) {
                        Err(SnapshotError::new(*status, reason.as_str()))
                    } else { Ok(None) };
                }
            }
            if !self.frontier_closed(&order) { return Ok(None); }
            if selected.contains_key(&id) { continue; }
            if store.location_candidate(&path, &scope, context)?.is_some() {
                budget.reserve(map_growth(&selected, 1) + id.len())?;
                selected.insert(id, path);
                if selected.len() == wanted.len() { return Ok(Some(selected)); }
            }
        }
        if let Some((order, status, reason, stale)) = issue {
            if *stale { return Ok(None); }
            if self.frontier_closed(order) { return Err(SnapshotError::new(*status, reason.as_str())); }
        }
        Ok(self.complete.then_some(selected))
    }

}

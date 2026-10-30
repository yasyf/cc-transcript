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
        let canonical = match Self::canonical_path(store,path) {
            Ok(canonical) => canonical,
            Err(error) => return Ok(Self { issue:Some((error.status,error.reason)), ..Self::empty() }),
        };
        store.authority(context,Some(&canonical))?;
        Ok(Self::metadata(canonical))
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

struct CatalogEdge {
    name: OsString,
    binding: CatalogBinding,
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
        self.canonical.capacity() + self.observed.payload_bytes() + self.payload_bytes
            + self.edges.capacity() * size_of::<CatalogEdge>()
            + (self.directories.capacity() + self.symlinks.capacity() + self.issues.capacity()) * size_of::<usize>()
            + self.scan.owned_bytes()
    }
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
        self.edge.name.capacity() + self.edge.binding.payload_bytes()
    }
}

struct CatalogPendingIssue {
    directory: SourceIdentity,
    status: Status,
    reason: String,
    position: usize,
    attempt: u64,
}

struct CatalogPendingRaw {
    directory: SourceIdentity,
    name: OsString,
    symlink: bool,
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
    pending_edge: Option<CatalogPendingEdge>,
    pending_raw: Option<CatalogPendingRaw>,
    pending_issue: Option<CatalogPendingIssue>,
    roots_bytes: usize,
    directory_bytes: usize,
    index_bytes: usize,
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
            + self.route_index.reserved_bytes() + self.route_bytes
            + self.check_bytes
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
            + self.pending_raw.as_ref().map_or(0, |raw| raw.name.capacity())
            + self.pending_edge.as_ref().map_or(0, CatalogPendingEdge::owned_bytes)
            + self.pending_issue.as_ref().map_or(0, |issue| issue.reason.capacity())
    }

    #[cfg(test)]
    fn audit_charge(&self) -> usize {
        size_of::<Self>()
            + self.roots.capacity() * size_of::<CatalogRoot>()
            + self.roots.iter().map(|root| root.path.capacity() + root.binding.payload_bytes()).sum::<usize>()
            + self.directories.reserved_bytes() + self.candidates.reserved_bytes()
            + self.directories.values().map(|node| {
                node.canonical.capacity() + node.observed.payload_bytes()
                    + node.edges.capacity() * size_of::<CatalogEdge>()
                    + node.edges.iter().map(|edge| edge.name.capacity() + edge.binding.payload_bytes()).sum::<usize>()
                    + (node.directories.capacity() + node.symlinks.capacity() + node.issues.capacity()) * size_of::<usize>()
                    + node.scan.owned_bytes()
            }).sum::<usize>()
            + self.candidates.iter().map(|(key, matches)| key.capacity() + matches.capacity() * size_of::<CatalogCandidate>()).sum::<usize>()
            + self.route_index.reserved_bytes()
            + self.route_index.values().map(CatalogRoute::owned_bytes).sum::<usize>()
            + self.checks.keys().map(|order| size_of::<(Vec<usize>,CatalogCheck)>() + order.capacity() * size_of::<usize>()).sum::<usize>()
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
            + self.pending_raw.as_ref().map_or(0, |raw| raw.name.capacity())
            + self.pending_edge.as_ref().map_or(0, CatalogPendingEdge::owned_bytes)
            + self.pending_issue.as_ref().map_or(0, |issue| issue.reason.capacity())
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
            directories:Table::new(work.clone()), candidates:Table::new(work.clone()),
            route_index:Table::new(work), route_bytes:0, routes_dirty:false, checks:BTreeMap::new(), check_bytes:0, retry_issues:Vec::new(), edge_issues:0, issue_bound:None, issues_dirty:false,
            pending:Vec::new(), pending_raw:None, pending_edge:None, pending_issue:None,
            directory_bytes:0, index_bytes:0, epoch:0, candidate_version:0, generation_version:0, directory_sequence:0, complete:false,
        })
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
            self.directories.growth_for(&stamp.identity) + 2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
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
        if !self.route_index.contains_key(&identity)
            && self.directories.get(&identity).is_some_and(|node| !node.edges.is_empty())
        { self.routes_dirty = true; }
        if let Some(old) = self.route_index.get(&identity) {
            if old.order <= route.order { return; }
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
            let node = self.directories.get_mut(&identity).expect("orphan catalog directory");
            let before = node.owned_bytes();
            node.edges.clear(); node.directories.clear(); node.symlinks.clear(); node.issues.clear();
            node.payload_bytes = 0; node.scan = CatalogScan::Gone;
            self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
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
        if self.pending_raw.as_ref().is_some_and(|edge| !routes.contains_key(&edge.directory)) { self.pending_raw = None; }
        if self.pending_edge.as_ref().is_some_and(|edge| !routes.contains_key(&edge.directory)) { self.pending_edge = None; }
        if self.pending_issue.as_ref().is_some_and(|issue| !routes.contains_key(&issue.directory)) { self.pending_issue = None; }
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
        if let Some(node) = self.directories.get_mut(&stamp.identity) {
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
            generation, canonical, observed, edges: Vec::new(), directories: Vec::new(), symlinks: Vec::new(), issues: Vec::new(),
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
        let has_issue = edge.binding.issue.as_ref().is_some_and(|(status, _)| *status != Status::Missing);
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
                self.directories.growth_for(&stamp.identity) + 2 * canonical.as_os_str().len() + vec_growth(&self.pending, 1)
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
        let directory = pending.directory;
        self.candidate_version += 1;
        if pending.symlink || pending.edge.binding.issue.as_ref().is_some_and(|(status,_)| *status != Status::Missing) {
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
        if pending.edge.binding.issue.as_ref().is_some_and(|(status, _)| *status != Status::Missing) {
            self.edge_issues += 1;
            node.issues.push(edge_index);
        }
        node.payload_bytes += pending.edge.name.capacity() + pending.edge.binding.payload_bytes();
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
        self.directory_sequence += 1;
        let node = self.directories.get_mut(&identity).expect("catalog issue directory");
        node.generation = self.directory_sequence;
        let before = node.owned_bytes();
        node.edges.clear(); node.directories.clear(); node.symlinks.clear(); node.issues.clear(); node.payload_bytes = 0;
        node.scan = CatalogScan::Queued;
        self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
        self.pending.push(identity);
        self.complete = false;
        self.routes_dirty = true;
        self.candidate_version += 1;
        self.generation_version += 1;
        self.sync_routes(budget)
    }

    fn advance(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        max_entries: usize, usage: &mut [u64;20], budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        self.advance_until(store, cancel, deadline, max_entries, usage, budget, None).map(|_| ())
    }

    fn advance_until(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        max_entries: usize, usage: &mut [u64;20], budget: &mut CatalogBudget<'_, '_>,
        mut query: Option<&mut CatalogLocateQuery>) -> Result<Option<CatalogChoice>, SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        let retries = if query.is_none() {
            self.retry_issues.iter().filter(|identity| self.directories.get(*identity).is_some_and(|node|
                matches!(node.scan,CatalogScan::Issue { attempt,.. } if attempt != budget.attempt))).count()
        } else { 0 };
        budget.reserve(vec_growth(&self.pending,retries))?;
        self.pending.reserve(retries);
        let mut position = 0;
        let mut requeued = false;
        while query.is_none() && position < self.retry_issues.len() {
            let identity = self.retry_issues[position];
            let issue = self.directories.get(&identity).and_then(|node| match node.scan {
                CatalogScan::Issue { attempt, .. } => Some(attempt), _ => None,
            });
            if issue.is_none() { self.retry_issues.swap_remove(position); continue; }
            if issue == Some(budget.attempt) { position += 1; continue; }
            self.retry_issues.swap_remove(position);
            self.remove_directory_index(identity);
            self.directory_sequence += 1;
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            node.generation = self.directory_sequence;
            let before = node.owned_bytes();
            node.edges.clear(); node.directories.clear(); node.symlinks.clear(); node.issues.clear(); node.payload_bytes = 0;
            node.scan = CatalogScan::Queued;
            self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
            self.pending.push(identity);
            requeued = true;
            self.complete = false;
        }
        if requeued { self.generation_version += 1; self.sort_pending(); }
        if self.issues_dirty { self.rebuild_issue_bound(); }
        if self.complete { return Ok(None); }
        let mut examined = 0;
        loop {
            cancel.check(deadline)?;
            if let Some(issue) = self.pending_issue.as_ref() {
                let route = self.route_index.get(&issue.directory).expect("catalog issue route");
                budget.reserve(issue.reason.capacity() + vec_growth(&self.retry_issues,1)
                    + Self::check_growth(route.order.len() + 1))?;
                let order = Self::query_order(route,issue.position,budget)?;
                let issue = self.pending_issue.take().expect("admitted catalog issue");
                let node = self.directories.get_mut(&issue.directory).expect("catalog directory");
                let before = node.owned_bytes();
                node.scan = CatalogScan::Issue { status:issue.status, reason:issue.reason,
                    position:issue.position, attempt:issue.attempt };
                self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
                self.pending.retain(|identity| *identity != issue.directory);
                self.retry_issues.push(issue.directory);
                self.record_issue(issue.directory,issue.position);
                self.install_check(order,CatalogCheck { kind:CatalogCheckKind::ReadIssue(issue.directory) });
                if query.is_some() {
                    let CatalogScan::Issue { status,reason,.. } = &self.directories.get(&issue.directory).expect("committed catalog issue").scan else {
                        unreachable!("catalog issue commit");
                    };
                    return Err(SnapshotError::new(*status,reason));
                }
                continue;
            }
            if let Some(raw) = self.pending_raw.take() {
                let path = catalog_child_path(&self.route_index.get(&raw.directory).expect("catalog pending route").path,&raw.name);
                let binding = match CatalogBinding::inspect(store, &path, budget.context) {
                    Ok(binding) => binding,
                    Err(error) => { self.pending_raw = Some(raw); return Err(error); }
                };
                self.pending_edge = Some(CatalogPendingEdge { directory:raw.directory,
                    edge:CatalogEdge { name:raw.name, binding }, symlink:raw.symlink });
            }
            if let Some(pending) = self.pending_edge.take() {
                if let Err(error) = self.install_edge(&pending,budget) {
                    self.pending_edge = Some(pending);
                    return Err(error);
                }
                let directory = pending.directory;
                let edge_index = self.directories.get(&directory).expect("catalog directory").edges.len();
                self.commit_edge(pending);
                let edge = &self.directories.get(&directory).expect("committed catalog directory").edges[edge_index];
                if query.is_some() {
                    if let Some((status,reason)) = &edge.binding.issue {
                        if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
                    }
                }
                let path = Path::new(&edge.name);
                let stem = path.file_stem().map(|stem| stem.to_string_lossy());
                let id = query.as_ref().and_then(|query|
                    (edge.binding.regular && path.extension().is_some_and(|extension| extension == "jsonl"))
                        .then(|| NativeStore::requested_location(stem.as_ref().expect("catalog source stem"),&query.wanted))
                        .flatten().filter(|id| !query.found.contains(*id)));
                let choice = if let Some(id) = id {
                    let route = self.route_index.get(&directory).expect("catalog candidate route");
                    budget.reserve(id.len() + route.path.as_os_str().len() + 1 + edge.name.len())?;
                    Some(CatalogChoice { id:id.to_owned(), path:catalog_child_path(&route.path,&edge.name),
                        order:Self::query_order(route,edge_index,budget)? })
                } else { None };
                if let Some(query) = query.as_deref_mut() {
                    let route = self.route_index.get(&directory).expect("catalog progress route");
                    let node = self.directories.get(&directory).expect("catalog capability directory");
                    if edge.binding.directory || node.symlinks.last() == Some(&edge_index) {
                        let order = Self::query_order(route,edge_index,budget)?;
                        let path = catalog_child_path(&route.path,&edge.name);
                        let generation = if node.symlinks.last() == Some(&edge_index) { node.generation }
                            else { self.directories.get(&edge.binding.stamp.expect("catalog child stamp").identity).expect("catalog child capability").generation };
                        query.remember_capability(order,&path,generation,&edge.binding,budget)?;
                    }
                    query.position(&route.order,Some(edge_index),budget)?;
                }
                if choice.is_some() { return Ok(choice); }
                continue;
            }
            if self.next_is_root() {
                let context = budget.context;
                let index = self.observe_root(store, context, budget)?;
                let root = &self.roots[index];
                let stem = root.path.file_stem().map(|stem| stem.to_string_lossy());
                let id = query.as_ref().and_then(|query|
                    (root.binding.regular && root.path.extension().is_some_and(|extension| extension == "jsonl"))
                        .then(|| NativeStore::requested_location(stem.as_ref().expect("catalog root stem"),&query.wanted))
                        .flatten().filter(|id| !query.found.contains(*id)));
                let choice = if let Some(id) = id {
                    budget.reserve(id.len() + root.path.as_os_str().len() + size_of::<usize>())?;
                    Some(CatalogChoice { id:id.to_owned(), path:root.path.clone(), order:vec![self.roots.len() - index - 1] })
                } else { None };
                if query.is_some() {
                    if let Some((status,reason)) = &root.binding.issue {
                        if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
                    }
                }
                if let Some(query) = query.as_deref_mut() {
                    budget.reserve(size_of::<usize>())?;
                    let generation = if root.binding.directory {
                        self.directories.get(&root.binding.stamp.expect("catalog root stamp").identity).expect("catalog root capability").generation
                    } else { 0 };
                    query.remember_capability(vec![self.roots.len() - index - 1],&root.path,generation,&root.binding,budget)?;
                    query.position(&[self.roots.len() - index - 1],None,budget)?;
                }
                if choice.is_some() { return Ok(choice); }
                continue;
            }
            let Some(identity) = self.pending.last().copied() else {
                self.epoch += 1;
                self.complete = true;
                return Ok(None);
            };
            let issue_order_len = self.route_index.get(&identity).expect("catalog issue route").order.len() + 1;
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            let before = node.owned_bytes();
            if matches!(node.scan, CatalogScan::Queued) {
                if query.is_some() { budget.reserve(Self::check_growth(issue_order_len))?; }
                store.authority(budget.context, Some(&node.canonical))?;
                budget.reserve(READ_DIR_HANDLE_BYTES + node.canonical.as_os_str().len() + FILESYSTEM_PATH_BYTES
                    + vec_growth(&self.retry_issues, 1))?;
                node.scan = match OpenDirectory::open(&node.canonical) {
                    Ok(reader) => CatalogScan::Open(reader),
                    Err(error) => CatalogScan::Issue { status: error.status, reason: error.reason, position: 0, attempt: budget.attempt },
                };
                self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
                if matches!(node.scan, CatalogScan::Issue { .. }) {
                    self.retry_issues.push(identity);
                    self.record_issue(identity, 0);
                    if query.is_some() {
                        let route = self.route_index.get(&identity).expect("catalog issue route");
                        let mut order = Vec::with_capacity(issue_order_len);
                        order.extend_from_slice(&route.order); order.push(0);
                        self.install_check(order,CatalogCheck { kind:CatalogCheckKind::ReadIssue(identity) });
                        self.pending.pop();
                        let CatalogScan::Issue { status,reason,.. } = &self.directories.get(&identity).expect("committed directory issue").scan else {
                            unreachable!("directory issue commit");
                        };
                        return Err(SnapshotError::new(*status,reason));
                    }
                    self.pending.pop();
                    continue;
                }
            }
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            let CatalogScan::Open(reader) = &mut node.scan else {
                self.pending.pop();
                continue;
            };
            if examined == max_entries { return Ok(None); }
            let item = reader.next();
            let before = node.owned_bytes();
            let entry = match item {
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    let error = io_error(error);
                    self.pending_issue = Some(CatalogPendingIssue { directory: identity, status: error.status,
                        reason: error.reason, position: node.edges.len(), attempt: budget.attempt });
                    continue;
                }
                None => {
                    node.scan = CatalogScan::Done;
                    self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
                    self.pending.pop();
                    continue;
                }
            };
            examined += 1;
            usage[17] += 1;
            let path = entry.path();
            let symlink = std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink());
            self.pending_raw = Some(CatalogPendingRaw { directory:identity, name:entry.file_name(), symlink });
            #[cfg(test)]
            store.after_location_metadata("entry");
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
        self.directory_sequence += 1;
        let node = self.directories.get_mut(&identity).expect("catalog directory");
        node.generation = self.directory_sequence;
        let before = node.owned_bytes();
        node.edges.clear();
        node.directories.clear();
        node.symlinks.clear();
        node.issues.clear();
        node.payload_bytes = 0;
        let same_directory = fresh.directory && fresh.stamp.is_some_and(|stamp| stamp.identity == identity);
        node.observed = fresh;
        node.scan = if same_directory { CatalogScan::Queued } else { CatalogScan::Gone };
        self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
        self.pending.retain(|pending| *pending != identity);
        if self.pending_raw.as_ref().is_some_and(|pending| pending.directory == identity) { self.pending_raw = None; }
        if self.pending_edge.as_ref().is_some_and(|pending| pending.directory == identity) {
            self.pending_edge = None;
        }
        if self.pending_issue.as_ref().is_some_and(|issue| issue.directory == identity) {
            self.pending_issue = None;
        }
        if same_directory { self.pending.push(identity); }
        self.complete = false;
        self.routes_dirty = true;
        self.epoch += 1;
        self.candidate_version += 1;
        self.generation_version += 1;
        Ok(())
    }

    fn check_roots(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        for index in 0..self.roots.len() {
            cancel.check(deadline)?;
            let root = &self.roots[index];
            let known = root.binding.canonical.is_some() || root.binding.issue.is_some();
            let fresh = CatalogBinding::inspect(store, &root.path, budget.context)?;
            if let Some((status, reason)) = &fresh.issue {
                self.complete = false;
            self.routes_dirty = true;
            self.epoch += 1;
                return Err(SnapshotError::new(*status, reason));
            }
            if root.binding.unchanged(&fresh) { continue; }
            if known { self.generation_version += 1; }
            budget.reserve(fresh.payload_bytes())?;
            if fresh.directory {
                budget.reserve(fresh.canonical.as_ref().expect("catalog root").as_os_str().len())?;
                self.schedule(fresh.canonical.as_ref().expect("catalog root").clone(), fresh.stamp.expect("catalog root stamp"), budget)?;
            }
            let root = &mut self.roots[index];
            self.roots_bytes = self.roots_bytes - root.binding.payload_bytes() + fresh.payload_bytes();
            root.binding = fresh;
            self.complete = false;
            self.routes_dirty = true;
            self.epoch += 1;
        }
        Ok(())
    }

    fn check_frontier(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        self.check_roots(store, cancel, deadline, budget)?;
        let Some(identity) = self.pending.last().copied() else { return Ok(()); };
        let node = self.directories.get(&identity).expect("catalog frontier directory");
        let fresh = match CatalogBinding::inspect(store, &node.canonical, budget.context) {
            Ok(fresh) => fresh,
            Err(error) if error.status == Status::PermissionDenied => return Ok(()),
            Err(error) => return Err(error),
        };
        if !node.observed.unchanged(&fresh) { self.invalidate_directory(identity, fresh, budget)?; }
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
            let mut dirty = !node.observed.unchanged(&fresh);
            for index in &node.symlinks {
                cancel.check(deadline)?;
                let edge = &node.edges[*index];
                let path = catalog_child_path(&node.canonical, &edge.name);
                match CatalogBinding::inspect(store, &path, budget.context) {
                    Ok(fresh) => dirty |= !edge.binding.unchanged(&fresh),
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
        if self.next_root == 0 && self.pending.is_empty() && self.pending_raw.is_none() && self.pending_edge.is_none() && self.pending_issue.is_none() {
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
                let node = self.directories.remove(&identity).expect("catalog directory");
                self.directory_bytes -= node.owned_bytes();
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
        self.routes_dirty = true;
        self.complete = false;
        self.epoch += 1;
        self.candidate_version += 1;
        self.generation_version += 1;
        Ok(())
    }

    fn hold_prefix(&mut self, store: &NativeStore, context: &Value, cancel: &Cancellation,
        deadline: u64, bound: Option<&Vec<usize>>, query: &mut CatalogLocateQuery,
        budget: &mut CatalogBudget<'_, '_>) -> Result<bool,SnapshotError> {
        budget.scratch(LOCATE_USER_PATH_SLOTS)?;
        loop {
            cancel.check(deadline)?;
            let lower = query.validated_after.as_ref().map_or(std::ops::Bound::Unbounded,std::ops::Bound::Excluded);
            let upper = bound.map_or(std::ops::Bound::Unbounded,std::ops::Bound::Included);
            let Some((order,check)) = self.checks.range::<Vec<usize>,_>((lower,upper)).next() else { return Ok(true); };
            let check = *check;
            if let CatalogCheckKind::ReadIssue(identity) = check.kind {
                let node = self.directories.get(&identity).expect("catalog issue directory");
                if let CatalogScan::Issue { status,reason,attempt,.. } = &node.scan {
                    if *attempt == budget.attempt { return Err(SnapshotError::new(*status,reason)); }
                    let parent = &self.route_index.get(&identity).expect("catalog issue route").order;
                    budget.reserve(parent.len() * size_of::<usize>())?;
                    let parent = parent.clone();
                    query.rewind_parent(&parent,budget)?;
                    self.restart_issue(identity,budget)?;
                    query.generation_seen = Some(self.generation_version);
                    return Ok(false);
                }
                unreachable!("catalog issue check without issue");
            }
            query.validation_position(order,budget)?;
            let order = query.validated_after.as_ref().expect("catalog validation position");
            let path = match check.kind {
                CatalogCheckKind::Root(index) => self.roots[index].path.clone(),
                CatalogCheckKind::Directory(identity) => self.route_index.get(&identity).expect("catalog check route").path.clone(),
                CatalogCheckKind::ReadIssue(_) => unreachable!("catalog issue handled before metadata"),
                CatalogCheckKind::Edge(identity,index) => {
                    let route = self.route_index.get(&identity).expect("catalog check parent route");
                    let node = self.directories.get(&identity).expect("catalog check parent");
                    catalog_child_path(&route.path,&node.edges[index].name)
                }
            };
            let old = match check.kind {
                CatalogCheckKind::Root(index) => &self.roots[index].binding,
                CatalogCheckKind::Directory(identity) => &self.directories.get(&identity).expect("catalog check directory").observed,
                CatalogCheckKind::Edge(identity,index) => &self.directories.get(&identity).expect("catalog check parent").edges[index].binding,
                CatalogCheckKind::ReadIssue(_) => unreachable!("catalog issue handled before metadata"),
            };
            let generation = match check.kind {
                CatalogCheckKind::Root(index) => {
                    let root = &self.roots[index];
                    if root.binding.directory { self.directories.get(&root.binding.stamp.expect("catalog root stamp").identity).expect("catalog root capability").generation }
                    else { 0 }
                }
                CatalogCheckKind::Directory(identity) | CatalogCheckKind::Edge(identity,_) =>
                    self.directories.get(&identity).expect("catalog check capability").generation,
                CatalogCheckKind::ReadIssue(_) => unreachable!("catalog issue handled before metadata"),
            };
            let fresh = match CatalogBinding::canonical_path(store,&path) {
                Ok(canonical) => {
                    if query.reuses_capability(order,&path,generation,&canonical,old) { continue; }
                    store.authority(context,Some(&canonical))?;
                    CatalogBinding::metadata(canonical)
                }
                Err(error) => CatalogBinding { issue:Some((error.status,error.reason)), ..CatalogBinding::empty() },
            };
            if let Some((status,reason)) = &fresh.issue {
                if *status != Status::Missing { return Err(SnapshotError::new(*status,reason)); }
            }
            if old.unchanged(&fresh) {
                budget.reserve(order.len() * size_of::<usize>())?;
                let capability_order = order.clone();
                query.remember_capability(capability_order,&path,generation,&fresh,budget)?;
                continue;
            }
            let parent = match check.kind {
                CatalogCheckKind::Root(_) => order,
                CatalogCheckKind::Directory(identity) | CatalogCheckKind::Edge(identity,_) =>
                    &self.route_index.get(&identity).expect("invalidated catalog route").order,
                CatalogCheckKind::ReadIssue(_) => unreachable!("catalog issue handled before metadata"),
            };
            budget.reserve(parent.len() * size_of::<usize>())?;
            let parent = parent.clone();
            query.rewind_parent(&parent,budget)?;
            match check.kind {
                CatalogCheckKind::Root(index) => self.update_root(index,fresh,budget)?,
                CatalogCheckKind::Directory(identity) => self.invalidate_directory(identity,fresh,budget)?,
                CatalogCheckKind::ReadIssue(_) => unreachable!("catalog issue handled before metadata"),
                CatalogCheckKind::Edge(identity,_) => {
                    let observed = &self.directories.get(&identity).expect("catalog check parent").observed;
                    budget.reserve(observed.payload_bytes())?;
                    self.invalidate_directory(identity,observed.clone(),budget)?;
                }
            }
            self.sync_routes(budget)?;
            query.generation_seen = Some(self.generation_version);
            return Ok(false);
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
                if let Some((status, reason)) = &edge.binding.issue {
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

struct CatalogBudget<'a, 'b> {
    store: &'a NativeStore,
    context: &'b Value,
    reservation: &'b mut ProjectionReservation<'a>,
    attempt: u64,
}

impl CatalogBudget<'_, '_> {
    fn reserve(&mut self, bytes: usize) -> Result<(), SnapshotError> {
        self.store.extend_projection_reservation(self.reservation, self.context, bytes)
    }
}

struct CatalogBinding {
    canonical: Option<PathBuf>,
    stamp: Option<SourceStamp>,
    directory: bool,
    regular: bool,
    issue: Option<(Status, String)>,
}

impl CatalogBinding {
    fn inspect(store: &NativeStore, path: &Path, context: &Value) -> Self {
        #[cfg(test)]
        store.location_canonicalizations.fetch_add(1, Ordering::Relaxed);
        let resolved = (|| {
            let canonical = std::fs::canonicalize(path).map_err(io_error)?;
            store.authority(context, Some(&canonical))?;
            let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
            Ok::<_, SnapshotError>((canonical, metadata))
        })();
        match resolved {
            Ok((canonical, metadata)) => Self {
                canonical: Some(canonical), stamp: Some(SourceStamp::of(&metadata)),
                directory: metadata.is_dir(), regular: metadata.is_file(), issue: None,
            },
            Err(error) => Self {
                canonical: None, stamp: None, directory: false, regular: false,
                issue: Some((error.status, error.reason)),
            },
        }
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

struct SourceCatalog {
    context: Value,
    roots: Vec<CatalogRoot>,
    directories: Table<SourceIdentity, CatalogDirectory>,
    candidates: Table<String, Vec<CatalogCandidate>>,
    route_index: Table<SourceIdentity, CatalogRoute>,
    route_bytes: usize,
    routes_dirty: bool,
    retry_issues: Vec<SourceIdentity>,
    edge_issues: usize,
    issue_bound: Option<(SourceIdentity, usize)>,
    issues_dirty: bool,
    pending: Vec<SourceIdentity>,
    pending_edge: Option<CatalogPendingEdge>,
    pending_issue: Option<CatalogPendingIssue>,
    context_bytes: usize,
    roots_bytes: usize,
    directory_bytes: usize,
    index_bytes: usize,
    epoch: u64,
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
        size_of::<Self>() + self.context_bytes + self.roots_bytes
            + self.roots.capacity() * size_of::<CatalogRoot>()
            + self.directories.reserved_bytes() + self.candidates.reserved_bytes()
            + self.directory_bytes + self.index_bytes
            + self.route_index.reserved_bytes() + self.route_bytes
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
            + self.pending_edge.as_ref().map_or(0, CatalogPendingEdge::owned_bytes)
            + self.pending_issue.as_ref().map_or(0, |issue| issue.reason.capacity())
    }

    #[cfg(test)]
    fn audit_charge(&self) -> usize {
        size_of::<Self>() + value_bytes(&self.context)
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
            + self.retry_issues.capacity() * size_of::<SourceIdentity>()
            + self.pending.capacity() * size_of::<SourceIdentity>()
            + self.pending_edge.as_ref().map_or(0, CatalogPendingEdge::owned_bytes)
            + self.pending_issue.as_ref().map_or(0, |issue| issue.reason.capacity())
    }

    fn new(store: &NativeStore, roots: &[Value], context: &Value, budget: &mut CatalogBudget<'_, '_>) -> Result<Self, SnapshotError> {
        let authority_len = encoded_size(&context["authority"], MAX_DATA_BYTES)?;
        budget.reserve(size_of::<Self>() + authority_len
            + dom_parse_bound(authority_len).expect("catalog authority allocation bound")
            + 256 + roots.len() * (size_of::<CatalogRoot>() + 2 * FILESYSTEM_PATH_BYTES))?;
        let mut authority_bytes = Vec::with_capacity(authority_len);
        sonic_rs::to_writer(&mut authority_bytes, &context["authority"]).expect("catalog authority serialization");
        let authority: Value = sonic_rs::from_slice(&authority_bytes).expect("serialized catalog authority");
        let roots = roots.iter().map(|root| {
            let path = PathBuf::from(root.as_str().ok_or_else(|| invalid("invalid root"))?);
            let binding = CatalogBinding::inspect(store, &path, context);
            if let Some((status, reason)) = &binding.issue {
                return Err(SnapshotError::new(*status, reason));
            }
            Ok(CatalogRoot { path, binding })
        }).collect::<Result<Vec<_>, SnapshotError>>()?;
        let work = store.lock_state().ledger.shared.work().clone();
        let mut catalog = Self {
            context: json!({"authority":authority}), roots,
            directories: Table::new(work.clone()), candidates: Table::new(work.clone()),
            route_index: Table::new(work), route_bytes: 0, routes_dirty: false, retry_issues: Vec::new(), edge_issues: 0, issue_bound: None, issues_dirty: false,
            pending: Vec::new(), pending_edge: None, pending_issue: None,
            context_bytes: 0, roots_bytes: 0, directory_bytes: 0, index_bytes: 0,
            epoch: 0, complete: false,
        };
        catalog.context_bytes = value_bytes(&catalog.context);
        catalog.roots_bytes = catalog.roots.iter().map(|root| root.path.capacity() + root.binding.payload_bytes()).sum();
        for index in 0..catalog.roots.len() {
            let root = &catalog.roots[index];
            if root.binding.directory {
                let canonical = root.binding.canonical.as_ref().expect("catalog root").clone();
                let stamp = root.binding.stamp.expect("catalog root stamp");
                catalog.schedule(canonical, stamp, budget)?;
                let root = &catalog.roots[index];
                let order = catalog.roots.len() - index - 1;
                budget.reserve(catalog.route_index.growth_for(&stamp.identity)
                    + root.path.as_os_str().len() + size_of::<usize>())?;
                catalog.install_route(stamp.identity, CatalogRoute { path: root.path.clone(), order: vec![order] });
            }
        }
        Ok(catalog)
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

    fn install_route(&mut self, identity: SourceIdentity, route: CatalogRoute) {
        if !self.route_index.contains_key(&identity)
            && self.directories.get(&identity).is_some_and(|node| !node.edges.is_empty())
        { self.routes_dirty = true; }
        if let Some(old) = self.route_index.get(&identity) {
            if old.order <= route.order { return; }
            self.route_bytes -= old.owned_bytes();
            if self.directories.get(&identity).is_some_and(|node| !node.directories.is_empty()) {
                self.routes_dirty = true;
            }
        }
        self.route_bytes += route.owned_bytes();
        self.route_index.insert(identity, route);
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
        if self.pending_edge.as_ref().is_some_and(|edge| !routes.contains_key(&edge.directory)) { self.pending_edge = None; }
        if self.pending_issue.as_ref().is_some_and(|issue| !routes.contains_key(&issue.directory)) { self.pending_issue = None; }
        self.edge_issues = routes.keys().map(|identity| nodes.get(identity).expect("catalog route node").issues.len()).sum();
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

    fn frontier_after(&self, bound: impl Iterator<Item = usize>) -> bool {
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
        if let Some(node) = self.directories.get_mut(&stamp.identity) {
            let before = node.owned_bytes();
            node.canonical = canonical;
            if matches!(node.scan, CatalogScan::Gone) {
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
            canonical, observed, edges: Vec::new(), directories: Vec::new(), symlinks: Vec::new(), issues: Vec::new(),
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
                + (route.order.len() + 1) * size_of::<usize>()
        } else { 0 };
        budget.reserve(route_growth + vec_growth(&node.edges, 1) + pending.owned_bytes()
            + if pending.symlink { vec_growth(&node.symlinks, 1) } else { 0 }
            + if edge.binding.directory { vec_growth(&node.directories, 1) } else { 0 }
            + if has_issue { vec_growth(&node.issues, 1) } else { 0 }
            + self.candidates.growth(new_ids) + index_growth + directory_growth)?;
        Ok(())
    }

    fn commit_edge(&mut self, pending: CatalogPendingEdge) {
        let directory = pending.directory;
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

    fn advance(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        max_entries: usize, usage: &mut [u64; 20], budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.reserve(LOCATE_USER_PATH_SLOTS)?;
        let mut position = 0;
        let mut requeued = false;
        while position < self.retry_issues.len() {
            let identity = self.retry_issues[position];
            let issue = self.directories.get(&identity).and_then(|node| match node.scan {
                CatalogScan::Issue { attempt, .. } => Some(attempt), _ => None,
            });
            if issue.is_none() { self.retry_issues.swap_remove(position); continue; }
            if issue == Some(budget.attempt) { position += 1; continue; }
            budget.reserve(vec_growth(&self.pending, 1))?;
            self.retry_issues.swap_remove(position);
            self.remove_directory_index(identity);
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            let before = node.owned_bytes();
            node.edges.clear(); node.directories.clear(); node.symlinks.clear(); node.issues.clear(); node.payload_bytes = 0;
            node.scan = CatalogScan::Queued;
            self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
            self.pending.push(identity);
            requeued = true;
            self.complete = false;
        }
        if requeued { self.sort_pending(); }
        if self.issues_dirty { self.rebuild_issue_bound(); }
        if self.complete { return Ok(()); }
        let mut examined = 0;
        loop {
            cancel.check(deadline)?;
            if let Some(issue) = self.pending_issue.take() {
                if let Err(error) = budget.reserve(issue.reason.capacity() + vec_growth(&self.retry_issues, 1)) {
                    self.pending_issue = Some(issue);
                    return Err(error);
                }
                let node = self.directories.get_mut(&issue.directory).expect("catalog directory");
                let before = node.owned_bytes();
                node.scan = CatalogScan::Issue { status: issue.status, reason: issue.reason,
                    position: issue.position, attempt: issue.attempt };
                self.directory_bytes = self.directory_bytes - before + node.owned_bytes();
                self.pending.retain(|identity| *identity != issue.directory);
                self.retry_issues.push(issue.directory);
                self.record_issue(issue.directory, issue.position);
                continue;
            }
            if let Some(pending) = self.pending_edge.take() {
                if let Err(error) = self.install_edge(&pending, budget) {
                    self.pending_edge = Some(pending);
                    return Err(error);
                }
                self.commit_edge(pending);
                continue;
            }
            let Some(identity) = self.pending.last().copied() else {
                self.epoch += 1;
                self.complete = true;
                return Ok(());
            };
            if examined == max_entries { return Ok(()); }
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            let before = node.owned_bytes();
            if matches!(node.scan, CatalogScan::Queued) {
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
                    self.pending.pop();
                    continue;
                }
            }
            let node = self.directories.get_mut(&identity).expect("catalog directory");
            let CatalogScan::Open(reader) = &mut node.scan else {
                self.pending.pop();
                continue;
            };
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
            let binding = CatalogBinding::inspect(store, &path, &self.context);
            self.pending_edge = Some(CatalogPendingEdge { directory: identity, edge: CatalogEdge { name: entry.file_name(), binding }, symlink });
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
        let node = self.directories.get_mut(&identity).expect("catalog directory");
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
        Ok(())
    }

    fn check_roots(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        budget.reserve(LOCATE_USER_PATH_SLOTS)?;
        for index in 0..self.roots.len() {
            cancel.check(deadline)?;
            let root = &self.roots[index];
            let fresh = CatalogBinding::inspect(store, &root.path, &self.context);
            if let Some((status, reason)) = &fresh.issue {
                self.complete = false;
            self.routes_dirty = true;
            self.epoch += 1;
                return Err(SnapshotError::new(*status, reason));
            }
            if root.binding.unchanged(&fresh) { continue; }
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
        let fresh = CatalogBinding::inspect(store, &node.canonical, &self.context);
        if !node.observed.unchanged(&fresh) { self.invalidate_directory(identity, fresh, budget)?; }
        Ok(())
    }

    fn refresh(&mut self, store: &NativeStore, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<(), SnapshotError> {
        self.check_roots(store, cancel, deadline, budget)?;
        let mut changed = Vec::new();
        for (identity, node) in self.directories.iter() {
            cancel.check(deadline)?;
            let fresh = CatalogBinding::inspect(store, &node.canonical, &self.context);
            let mut dirty = !node.observed.unchanged(&fresh);
            for index in &node.symlinks {
                cancel.check(deadline)?;
                let edge = &node.edges[*index];
                let path = catalog_child_path(&node.canonical, &edge.name);
                let fresh = CatalogBinding::inspect(store, &path, &self.context);
                dirty |= !edge.binding.unchanged(&fresh);
            }
            if dirty {
                budget.reserve(vec_growth(&changed, 1) + fresh.payload_bytes())?;
                changed.push((*identity, fresh));
            }
        }
        for (identity, fresh) in changed { self.invalidate_directory(identity, fresh, budget)?; }
        self.sync_routes(budget)?;
        if self.pending.is_empty() && self.pending_edge.is_none() && self.pending_issue.is_none() {
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

    fn read_plan(&self, cancel: &Cancellation, deadline: u64,
        budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogReadPlan, SnapshotError> {
        budget.reserve(self.roots.len() * size_of::<PathBuf>()
            + self.roots.iter().map(|root| root.path.as_os_str().len()).sum::<usize>())?;
        let scope = self.roots.iter().map(|root| root.path.clone()).collect();
        let mut issue = None;
        for (identity, route) in self.route_index.iter() {
            cancel.check(deadline)?;
            let node = self.directories.get(identity).expect("catalog directory");
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
        let routes = &self.route_index;
        let scope = &plan.scope;
        let cached = self.cached_paths(store, ids, budget)?;
        let mut choices: Vec<(Vec<usize>, &str, PathBuf)> = Vec::new();
        for (index, root) in self.roots.iter().enumerate() {
            if !root.binding.regular || root.path.extension().is_none_or(|extension| extension != "jsonl") { continue; }
            let stem = root.path.file_stem().expect("catalog root stem").to_string_lossy();
            if let Some(id) = NativeStore::requested_location(&stem, &wanted) {
                budget.reserve(vec_growth(&choices, 1) + size_of::<usize>() + root.path.as_os_str().len())?;
                choices.push((vec![self.roots.len() - index - 1], id, root.path.clone()));
            }
        }
        for id in &wanted {
            if let Some(matches) = self.candidates.get(id.as_str()) {
                for candidate in matches {
                    cancel.check(deadline)?;
                    let Some(route) = routes.get(&candidate.directory) else { continue; };
                    let node = self.directories.get(&candidate.directory).expect("catalog directory");
                    let edge = &node.edges[candidate.edge];
                    let stem = Path::new(&edge.name).file_stem().expect("catalog source stem").to_string_lossy();
                    if NativeStore::requested_location(&stem, &wanted) != Some(id.as_str()) { continue; }
                    budget.reserve(vec_growth(&choices, 1) + route.path.as_os_str().len() + 1 + edge.name.len())?;
                    choices.push((Self::query_order(route, candidate.edge, budget)?, id.as_str(), catalog_child_path(&route.path, &edge.name)));
                }
            }
        }
        choices.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let issue = &plan.issue;
        budget.reserve(set_growth(&HashSet::<&str>::new(), ids.len()))?;
        let mut possible: HashSet<&str> = HashSet::with_capacity(ids.len());
        possible.extend(cached.keys().map(String::as_str));
        let mut feasible = possible.len() == wanted.len();
        for (order, id, _) in &choices {
            if feasible { break; }
            if let Some((error_order, _, _, _)) = &issue {
                if error_order <= order && self.frontier_closed(error_order) { feasible = true; break; }
            }
            if !self.frontier_closed(order) { break; }
            possible.insert(id);
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
        for (order, id, path) in choices {
            cancel.check(deadline)?;
            if let Some((error_order, status, reason, stale)) = &issue {
                if error_order <= &order {
                    return if !*stale && self.frontier_closed(error_order) {
                        Err(SnapshotError::new(*status, reason.as_str()))
                    } else { Ok(None) };
                }
            }
            if !self.frontier_closed(&order) { return Ok(None); }
            if selected.contains_key(id) { continue; }
            if store.location_candidate(&path, &scope, context)?.is_some() {
                budget.reserve(map_growth(&selected, 1) + id.len())?;
                selected.insert(id.to_owned(), path);
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

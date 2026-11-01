struct MembershipLocate {
    chunk_size: usize,
    next: usize,
    paths: HashMap<String, PathBuf>,
    revision: Option<CatalogCertificate>,
}

impl MembershipLocate {
    fn heap_bytes(&self) -> usize {
        self.paths.capacity() * size_of::<(String, PathBuf)>()
            + self
                .paths
                .iter()
                .map(|(id, path)| id.capacity() + path.capacity())
                .sum::<usize>()
    }
}

enum MembershipListing {
    Reading {
        entries: OpenDirectory,
        children: Vec<PathBuf>,
        pending: Option<OpenDirectoryEntry>,
    },
    Selected {
        children: Vec<PathBuf>,
        next: usize,
    },
}

impl MembershipListing {
    fn heap_bytes(&self) -> usize {
        let (reader, children) = match self {
            Self::Reading {
                entries,
                children,
                pending,
            } => (
                entries.retained_bytes()
                    + pending.as_ref().map_or(0, |entry| entry.name.capacity()),
                children,
            ),
            Self::Selected { children, .. } => (0, children),
        };
        reader
            + children.capacity() * size_of::<PathBuf>()
            + children.iter().map(PathBuf::capacity).sum::<usize>()
    }
}

#[derive(Clone, Copy)]
enum MembershipPhase {
    Locate,
    Sources,
    Sidechains,
    Finish,
}

struct MembershipBuild {
    locate: MembershipLocate,
    phase: MembershipPhase,
    source_next: usize,
    members: Vec<PreparedSourceRef>,
    seen: HashSet<SourceIdentity>,
    sidechain_dirs: Vec<(PathBuf, Option<SourceStamp>)>,
    examined: usize,
    listing: Option<MembershipListing>,
    catalog: Option<CatalogCertificate>,
}

impl MembershipBuild {
    fn new(chunk_size: usize) -> Self {
        Self {
            locate: MembershipLocate {
                chunk_size,
                next: 0,
                paths: HashMap::new(),
                revision: None,
            },
            phase: MembershipPhase::Locate,
            source_next: 0,
            members: Vec::new(),
            seen: HashSet::new(),
            sidechain_dirs: Vec::new(),
            examined: 0,
            listing: None,
            catalog: None,
        }
    }

    fn heap_bytes(&self) -> usize {
        size_of::<Self>()
            + self.locate.heap_bytes()
            + self.members.capacity() * size_of::<PreparedSourceRef>()
            + self
                .members
                .iter()
                .map(|source| source.path.capacity())
                .sum::<usize>()
            + self.seen.capacity() * size_of::<SourceIdentity>()
            + self.sidechain_dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + self
                .sidechain_dirs
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + self
                .listing
                .as_ref()
                .map_or(0, MembershipListing::heap_bytes)
    }
}

enum MembershipCell {
    Empty,
    Pending(Box<MembershipBuild>),
    Collected(Arc<WarmMembership>),
    Positive(Arc<WarmMembership>),
    Busy,
}

struct MembershipSlot {
    cell: Mutex<MembershipCell>,
    accounted: AtomicUsize,
    pending: AtomicBool,
    expires: AtomicU64,
}

impl Charge<String> for Arc<MembershipSlot> {
    fn key_charge(key: &String) -> usize {
        key.capacity()
    }

    fn charge(&self) -> usize {
        arc_bytes::<MembershipSlot>() + MUTEX_STORAGE_BYTES + self.accounted.load(Ordering::Acquire)
    }
}

impl MembershipSlot {
    #[cfg(test)]
    fn metadata(&self) -> Option<Arc<WarmMembership>> {
        match &*self.cell.lock().expect("registered membership") {
            MembershipCell::Collected(membership) | MembershipCell::Positive(membership) => {
                Some(Arc::clone(membership))
            }
            MembershipCell::Empty | MembershipCell::Pending(_) | MembershipCell::Busy => None,
        }
    }

    #[cfg(test)]
    fn positive(&self) -> Option<Arc<WarmMembership>> {
        match &*self.cell.lock().expect("registered membership") {
            MembershipCell::Positive(membership) => Some(Arc::clone(membership)),
            MembershipCell::Empty
            | MembershipCell::Pending(_)
            | MembershipCell::Collected(_)
            | MembershipCell::Busy => None,
        }
    }
}

fn membership_identities(membership: &Arc<WarmMembership>) -> [usize; 4] {
    [
        Arc::as_ptr(membership) as usize,
        slice_key(&membership.members),
        slice_key(&membership.positions),
        slice_key(&membership.sidechain_dirs),
    ]
}

fn membership_anchor(membership: &Arc<WarmMembership>) -> Anchor {
    Anchor::warm(
        Arc::as_ptr(membership) as usize,
        arc_bytes::<WarmMembership>() + membership.revision.capacity(),
    )
}

fn membership_anchors(membership: &Arc<WarmMembership>) -> impl Iterator<Item = Anchor> {
    std::iter::once(membership_anchor(membership)).chain(membership.anchors())
}

struct RetainedMembership<'a> {
    store: &'a NativeStore,
    membership: Option<Arc<WarmMembership>>,
    identities: [usize; 4],
}

impl std::ops::Deref for RetainedMembership<'_> {
    type Target = WarmMembership;

    fn deref(&self) -> &Self::Target {
        self.membership
            .as_ref()
            .expect("retained membership is live")
    }
}

impl RetainedMembership<'_> {
    #[cfg(test)]
    fn arc(&self) -> &Arc<WarmMembership> {
        self.membership
            .as_ref()
            .expect("retained membership is live")
    }
}

impl Drop for RetainedMembership<'_> {
    fn drop(&mut self) {
        let mut state = self.store.lock_state();
        let membership = self.membership.take().expect("retained membership is live");
        for identity in self.identities {
            state.ledger.shared.release(identity);
        }
        #[cfg(test)]
        {
            let registered = state
                .retained_membership_owners
                .iter()
                .position(|owner| owner.as_ptr() == Arc::as_ptr(&membership))
                .expect("retained membership is registered");
            state.retained_membership_owners.swap_remove(registered);
        }
        drop(membership);
    }
}

impl StoreState {
    #[cfg(test)]
    fn positive_membership(&self, key: &str) -> Option<Arc<WarmMembership>> {
        self.warm_memberships
            .get(key)
            .and_then(|slot| slot.positive())
    }

    fn retained_membership<'a>(
        &mut self,
        store: &'a NativeStore,
        membership: Arc<WarmMembership>,
    ) -> RetainedMembership<'a> {
        let identities = membership_identities(&membership);
        for identity in identities {
            self.ledger.shared.retain(identity);
        }
        #[cfg(test)]
        self.retained_membership_owners
            .push(Arc::downgrade(&membership));
        RetainedMembership {
            store,
            membership: Some(membership),
            identities,
        }
    }
}

impl NativeStore {
    #[cfg(test)]
    fn membership_checkpoint(
        &self,
        stage: &'static str,
        ordinal: usize,
    ) -> Result<(), SnapshotError> {
        let hook = self
            .membership_hook
            .lock()
            .expect("membership test hook")
            .clone();
        if let Some(hook) = hook {
            hook(stage, ordinal)?;
        }
        Ok(())
    }

    fn begin_membership_refresh(
        &self,
        slot: &MembershipSlot,
        context: &Value,
    ) -> Result<(), SnapshotError> {
        let state = self.lock_state();
        let pending = state
            .warm_memberships
            .values()
            .filter(|slot| slot.pending.load(Ordering::Acquire))
            .count();
        if pending >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "membership work admission exhausted",
            ));
        }
        slot.pending.store(true, Ordering::Release);
        Ok(())
    }

    fn certify_collected_membership(
        &self,
        membership: &WarmMembership,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        remaining: &mut WorkLimits,
    ) -> Result<(), SnapshotError> {
        if !self.catalog_membership_valid(
            membership.catalog,
            request,
            context,
            cancel,
            remaining,
        )? || !self.validate_warm_membership(
            &membership.members,
            &membership.sidechain_dirs,
            context,
            cancel,
            remaining.deadline_unix_ms,
            None,
        )? || !self.catalog_membership_valid(
            membership.catalog,
            request,
            context,
            cancel,
            remaining,
        )? {
            return Err(SnapshotError::new(
                Status::Changed,
                "registered membership changed during preparation",
            ));
        }
        let since = active_since_ns(request)?;
        if membership
            .members
            .iter()
            .filter(|member| active(member, since))
            .count()
            > remaining.max_sources
        {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered source bound exhausted",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn audit_membership_build_bytes(build: &MembershipBuild) -> usize {
        let MembershipBuild {
            locate,
            phase: _,
            source_next: _,
            members,
            seen,
            sidechain_dirs,
            examined: _,
            listing,
            catalog: _,
        } = build;
        let listing = listing.as_ref().map_or(0, |listing| {
            let (reader, names, pending) = match listing {
                MembershipListing::Reading {
                    entries,
                    children,
                    pending,
                } => (
                    entries.retained_bytes(),
                    children,
                    pending.as_ref().map_or(0, |entry| entry.name.capacity()),
                ),
                MembershipListing::Selected { children, .. } => (0, children, 0),
            };
            reader
                + pending
                + names.capacity() * size_of::<PathBuf>()
                + names.iter().map(PathBuf::capacity).sum::<usize>()
        });
        size_of::<MembershipBuild>()
            + locate.paths.capacity() * size_of::<(String, PathBuf)>()
            + locate
                .paths
                .iter()
                .map(|(key, path)| key.capacity() + path.capacity())
                .sum::<usize>()
            + members.capacity() * size_of::<PreparedSourceRef>()
            + members
                .iter()
                .map(|source| source.path.capacity())
                .sum::<usize>()
            + seen.capacity() * size_of::<SourceIdentity>()
            + sidechain_dirs.capacity() * size_of::<(PathBuf, Option<SourceStamp>)>()
            + sidechain_dirs
                .iter()
                .map(|(path, _)| path.capacity())
                .sum::<usize>()
            + listing
    }

    fn new_membership_build<'a>(
        &'a self,
        slot: &MembershipSlot,
        context: &Value,
        remaining: &WorkLimits,
        reservation: &mut ProjectionReservation<'a>,
    ) -> Result<Box<MembershipBuild>, SnapshotError> {
        let mut state = self.lock_state();
        let pending = state
            .warm_memberships
            .values()
            .filter(|slot| slot.pending.load(Ordering::Acquire))
            .count();
        if !slot.pending.load(Ordering::Acquire) && pending >= self.lease_cap(context)? {
            return Err(SnapshotError::new(
                Status::LeaseLimit,
                "membership work admission exhausted",
            ));
        }
        self.extend_projection_reservation_in(
            &mut state,
            reservation,
            context,
            size_of::<MembershipBuild>(),
        )?;
        slot.pending.store(true, Ordering::Release);
        #[cfg(test)]
        self.warm_records.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MembershipBuild::new(
            1024.min(remaining.max_items),
        )))
    }

    fn membership_slot<'a>(
        &'a self,
        key: &String,
        context: &Value,
        reservation: &mut ProjectionReservation<'a>,
    ) -> Result<Arc<MembershipSlot>, SnapshotError> {
        let mut state = self.lock_state();
        Self::prune(&mut state);
        if let Some(slot) = state.warm_memberships.get(key) {
            return Ok(Arc::clone(slot));
        }
        if state.warm_memberships.len() >= 32 {
            let oldest = state
                .warm_memberships
                .iter()
                .filter(|(_, slot)| Arc::strong_count(slot) == 1)
                .min_by_key(|(_, slot)| slot.expires.load(Ordering::Acquire))
                .map(|(key, _)| key.clone())
                .ok_or_else(|| {
                    SnapshotError::new(Status::LeaseLimit, "membership owners are in use")
                })?;
            state.remove_warm_membership(&oldest);
        }
        let additional = key.capacity()
            + arc_bytes::<MembershipSlot>()
            + MUTEX_STORAGE_BYTES
            + state.warm_memberships.growth_for(key);
        self.extend_projection_reservation_in(&mut state, reservation, context, additional)?;
        let slot = Arc::new(MembershipSlot {
            cell: Mutex::new(MembershipCell::Empty),
            accounted: AtomicUsize::new(0),
            pending: AtomicBool::new(false),
            expires: AtomicU64::new(now_ms().saturating_add(self.config.ttl)),
        });
        state.warm_memberships.reserve_for(key);
        state
            .warm_memberships
            .insert(key.clone(), Arc::clone(&slot));
        state.transient_bytes -= additional;
        reservation.bytes -= additional;
        Ok(slot)
    }

    fn membership_work<'a>(
        &'a self,
        key: &String,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        remaining: &mut WorkLimits,
        revalidate: bool,
    ) -> Result<RetainedMembership<'a>, SnapshotError> {
        let mut reservation = ProjectionReservation {
            store: self,
            bytes: 0,
        };
        let slot = self.membership_slot(key, context, &mut reservation)?;
        let mut cell = {
            let mut state = self.lock_state();
            loop {
                cancel.check(remaining.deadline_unix_ms)?;
                let mut cell = slot.cell.lock().expect("registered membership");
                if matches!(*cell, MembershipCell::Busy) {
                    drop(cell);
                    #[cfg(test)]
                    self.membership_checkpoint("wait", 0)?;
                    state = self
                        .fact_read_released
                        .wait_timeout(state, FACT_READ_WAIT_SLICE)
                        .expect("registered membership")
                        .0;
                    continue;
                }
                break std::mem::replace(&mut *cell, MembershipCell::Busy);
            }
        };
        let prior = match &cell {
            MembershipCell::Collected(membership) | MembershipCell::Positive(membership) => {
                Some(Arc::clone(membership))
            }
            _ => None,
        };
        let outcome = (|| {
            if let MembershipCell::Collected(collected) = &cell {
                let membership = Arc::clone(collected);
                self.certify_collected_membership(
                    &membership,
                    request,
                    context,
                    cancel,
                    remaining,
                )?;
                cell = MembershipCell::Positive(Arc::clone(&membership));
                return Ok(membership);
            }
            if let MembershipCell::Positive(cached) = &cell {
                let namespace_valid = self.catalog_membership_valid(
                    cached.catalog,
                    request,
                    context,
                    cancel,
                    remaining,
                )?;
                let absences = if namespace_valid && revalidate {
                    self.sidechain_absences(
                        cached.catalog,
                        &cached.sidechain_dirs,
                        context,
                        cancel,
                        remaining.deadline_unix_ms,
                    )?
                } else {
                    None
                };
                if namespace_valid
                    && (!revalidate
                        || self.validate_warm_membership(
                            &cached.members,
                            &cached.sidechain_dirs,
                            context,
                            cancel,
                            remaining.deadline_unix_ms,
                            absences.as_ref(),
                        )?)
                {
                    if !self.catalog_membership_valid(
                        cached.catalog,
                        request,
                        context,
                        cancel,
                        remaining,
                    )? {
                        return Err(SnapshotError::new(
                            Status::Changed,
                            "registered catalog changed during member validation",
                        ));
                    }
                    let since = active_since_ns(request)?;
                    if cached
                        .members
                        .iter()
                        .filter(|member| active(member, since))
                        .count()
                        > remaining.max_sources
                    {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "registered source bound exhausted",
                        ));
                    }
                    return Ok(Arc::clone(cached));
                }
                drop(absences);
                if namespace_valid {
                    self.begin_membership_refresh(&slot, context)?;
                    if let Some((membership, refreshed)) = self.refreshed_warm_membership(
                        key, cached, request, context, cancel, remaining,
                    )? {
                        Self::absorb_membership_reservation(&mut reservation, refreshed);
                        self.extend_projection_reservation(
                            &mut reservation,
                            context,
                            arc_bytes::<WarmMembership>(),
                        )?;
                        let membership = Arc::new(membership);
                        cell = MembershipCell::Positive(Arc::clone(&membership));
                        return Ok(membership);
                    }
                }
                cell = MembershipCell::Empty;
            }
            if matches!(cell, MembershipCell::Empty) {
                cell = MembershipCell::Pending(self.new_membership_build(
                    &slot,
                    context,
                    remaining,
                    &mut reservation,
                )?);
            }
            let namespace = match &cell {
                MembershipCell::Pending(build) => build.catalog.or(build.locate.revision),
                _ => None,
            };
            if let Some(certificate) = namespace {
                if !self.catalog_membership_valid(
                    Some(certificate),
                    request,
                    context,
                    cancel,
                    remaining,
                )? {
                    self.retire_membership_generation(key, &slot, &mut cell);
                    cell = MembershipCell::Pending(self.new_membership_build(
                        &slot,
                        context,
                        remaining,
                        &mut reservation,
                    )?);
                }
            }
            let MembershipCell::Pending(build) = &mut cell else {
                unreachable!("membership work was claimed")
            };
            let membership = self.advance_membership(
                build,
                request,
                context,
                cancel,
                usage,
                remaining,
                &mut reservation,
            )?;
            let membership = Arc::new(membership);
            cell = MembershipCell::Collected(Arc::clone(&membership));
            self.certify_collected_membership(&membership, request, context, cancel, remaining)?;
            cell = MembershipCell::Positive(Arc::clone(&membership));
            Ok(membership)
        })();
        self.restore_membership(key, &slot, cell, prior, outcome, context, &mut reservation)
    }

    fn retire_membership_generation(
        &self,
        key: &String,
        slot: &Arc<MembershipSlot>,
        cell: &mut MembershipCell,
    ) {
        let mut state = self.lock_state();
        let retired = std::mem::replace(cell, MembershipCell::Empty);
        assert!(matches!(retired, MembershipCell::Pending(_)));
        drop(retired);
        let record = state
            .warm_memberships
            .get_mut(key)
            .expect("registered membership owner");
        assert!(Arc::ptr_eq(&record, slot));
        record.accounted.store(0, Ordering::Release);
        record.pending.store(false, Ordering::Release);
    }

    fn restore_membership<'a>(
        &'a self,
        key: &String,
        slot: &Arc<MembershipSlot>,
        mut cell: MembershipCell,
        prior: Option<Arc<WarmMembership>>,
        mut outcome: Result<Arc<WarmMembership>, SnapshotError>,
        context: &Value,
        reservation: &mut ProjectionReservation<'a>,
    ) -> Result<RetainedMembership<'a>, SnapshotError> {
        if outcome.as_ref().is_err_and(|error| {
            !matches!(
                error.status,
                Status::Deadline
                    | Status::Cancelled
                    | Status::Incomplete
                    | Status::RetainedLimit
                    | Status::LeaseLimit
            )
        }) {
            cell = MembershipCell::Empty;
        }
        let mut state = self.lock_state();
        let catalog_pin = if let MembershipCell::Positive(membership) = &cell {
            membership
                .catalog
                .map(|certificate| {
                    let catalog = state.catalogs.get(certificate.key()).ok_or_else(|| {
                        SnapshotError::new(
                            Status::Changed,
                            "registered catalog was evicted before publication",
                        )
                    })?;
                    let catalog = Arc::clone(catalog);
                    let valid = matches!(&*catalog.cell.lock().expect("source catalog"),
                    CatalogCell::Idle(catalog) if certificate.matches(catalog));
                    if !valid {
                        return Err(SnapshotError::new(
                            Status::Changed,
                            "registered catalog changed before publication",
                        ));
                    }
                    Ok(catalog)
                })
                .transpose()
        } else {
            Ok(None)
        };
        let catalog_pin = match catalog_pin {
            Ok(pin) => pin,
            Err(error) => {
                outcome = Err(error);
                cell = MembershipCell::Empty;
                None
            }
        };
        let same_owner = match (&cell, &prior) {
            (
                MembershipCell::Collected(current) | MembershipCell::Positive(current),
                Some(prior),
            ) => Arc::ptr_eq(current, prior),
            _ => false,
        };
        if let MembershipCell::Collected(membership) | MembershipCell::Positive(membership) = &cell
        {
            if !same_owner {
                let growth = state.ledger.shared.growth(membership_anchors(membership));
                if let Err(error) =
                    self.extend_projection_reservation_in(&mut state, reservation, context, growth)
                {
                    outcome = Err(error);
                    cell = MembershipCell::Empty;
                }
            }
        }
        let old_bytes = slot.accounted.load(Ordering::Acquire);
        let new_bytes = match &cell {
            MembershipCell::Pending(build) => build.heap_bytes(),
            MembershipCell::Empty | MembershipCell::Collected(_) | MembershipCell::Positive(_) => 0,
            MembershipCell::Busy => unreachable!("membership work was returned"),
        };
        let shared_bytes = if same_owner {
            0
        } else {
            match &cell {
                MembershipCell::Collected(membership) | MembershipCell::Positive(membership) => {
                    state
                        .ledger
                        .shared
                        .admission(membership_anchors(membership))
                }
                _ => 0,
            }
        };
        let added = (new_bytes + shared_bytes).saturating_sub(old_bytes);
        assert!(
            added <= reservation.bytes,
            "membership outgrew admitted work"
        );
        state.transient_bytes -= added;
        reservation.bytes -= added;
        if !same_owner {
            if let MembershipCell::Collected(membership) | MembershipCell::Positive(membership) =
                &cell
            {
                state.ledger.shared.reserve(membership_anchors(membership));
                for anchor in membership_anchors(membership) {
                    state.ledger.shared.acquire(anchor);
                }
            }
        }
        if let Some(prior) = prior {
            if !same_owner {
                for identity in membership_identities(&prior) {
                    state.ledger.shared.release(identity);
                }
            }
            drop(prior);
        }
        let expires = match &cell {
            MembershipCell::Positive(membership) => membership.expires,
            _ => now_ms().saturating_add(self.config.ttl),
        };
        {
            let record = state
                .warm_memberships
                .get_mut(key)
                .expect("registered membership owner");
            assert!(
                Arc::ptr_eq(&record, slot),
                "registered membership owner changed"
            );
            record.accounted.store(new_bytes, Ordering::Release);
            record.pending.store(
                matches!(
                    cell,
                    MembershipCell::Pending(_) | MembershipCell::Collected(_)
                ),
                Ordering::Release,
            );
            record.expires.store(expires, Ordering::Release);
        }
        *slot.cell.lock().expect("registered membership") = cell;
        let result = outcome.map(|membership| state.retained_membership(self, membership));
        drop(catalog_pin);
        drop(state);
        self.fact_read_released.notify_all();
        result
    }

    fn absorb_membership_reservation(
        target: &mut ProjectionReservation<'_>,
        mut source: ProjectionReservation<'_>,
    ) {
        assert!(std::ptr::eq(target.store, source.store));
        target.bytes += source.bytes;
        source.bytes = 0;
    }

    fn advance_membership<'a>(
        &'a self,
        build: &mut MembershipBuild,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
        remaining: &mut WorkLimits,
        reservation: &mut ProjectionReservation<'a>,
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
        if matches!(build.phase, MembershipPhase::Locate) {
            match self.catalog_access(
                CatalogInput { ids, roots },
                context,
                cancel,
                remaining,
                usage,
                CatalogMode::Locate(&mut build.locate),
            )? {
                CatalogResolution::Ready {
                    paths,
                    reservation: selected,
                    certificate,
                } => {
                    Self::absorb_membership_reservation(reservation, selected);
                    build.locate.paths = paths;
                    build.catalog = certificate;
                    build.phase = MembershipPhase::Sources;
                }
                CatalogResolution::Stopped {
                    error,
                    reservation: selected,
                } => {
                    Self::absorb_membership_reservation(reservation, selected);
                    return Err(error);
                }
                _ => unreachable!("membership catalog selection"),
            }
        }
        let mut budget = CatalogBudget {
            store: self,
            context,
            reservation,
            attempt: self.sequence.fetch_add(1, Ordering::Relaxed) as u64,
            scratch_bytes: 0,
        };
        let mut listing_validated = false;
        loop {
            cancel.check(remaining.deadline_unix_ms)?;
            match build.phase {
                MembershipPhase::Locate => unreachable!("membership paths selected"),
                MembershipPhase::Sources => {
                    self.membership_source_step(build, ids, direct, &mut budget)?
                }
                MembershipPhase::Sidechains => self.membership_sidechain_step(
                    build,
                    &mut listing_validated,
                    remaining,
                    usage,
                    &mut budget,
                )?,
                MembershipPhase::Finish => {
                    if !self.catalog_membership_valid(
                        build.catalog,
                        request,
                        context,
                        cancel,
                        remaining,
                    )? {
                        return Err(SnapshotError::new(
                            Status::Changed,
                            "registered catalog changed during member preparation",
                        ));
                    }
                    if build.locate.paths.len() != ids.len() {
                        return Err(SnapshotError::new(
                            Status::Incomplete,
                            "registered membership is incomplete",
                        ));
                    }
                    budget.reserve(
                        arc_bytes::<WarmMembership>()
                            + 2 * <Sha256 as Digest>::output_size()
                            + arc_slice_bytes::<PreparedSourceRef>(build.members.len())
                            + arc_slice_bytes::<(SourceIdentity, usize)>(build.members.len())
                            + arc_slice_bytes::<(PathBuf, Option<SourceStamp>)>(
                                build.sidechain_dirs.len(),
                            ),
                    )?;
                    let revision = Self::warm_membership_revision(&build.members);
                    let positions = member_positions(&build.members);
                    return Ok(WarmMembership {
                        catalog: build.catalog,
                        revision,
                        positions,
                        members: std::mem::take(&mut build.members).into(),
                        sidechain_dirs: std::mem::take(&mut build.sidechain_dirs).into(),
                        complete: build.locate.paths.len() == ids.len(),
                        expires: now_ms().saturating_add(30 * 60_000),
                    });
                }
            }
        }
    }

    fn membership_source_step(
        &self,
        build: &mut MembershipBuild,
        ids: &[Value],
        direct: &[Value],
        budget: &mut CatalogBudget<'_, '_>,
    ) -> Result<(), SnapshotError> {
        if build.source_next == ids.len() + direct.len() {
            build.phase = MembershipPhase::Sidechains;
            return Ok(());
        }
        let path = if build.source_next < ids.len() {
            ids[build.source_next]
                .as_str()
                .and_then(|id| build.locate.paths.get(id))
                .map(PathBuf::as_path)
        } else {
            direct[build.source_next - ids.len()]
                .as_str()
                .map(Path::new)
        };
        if let Some(path) = path {
            budget.scratch(3 * FILESYSTEM_PATH_BYTES)?;
            let canonical = self.canonical_path(path).map_err(io_error)?;
            self.authority(budget.context, Some(&canonical))?;
            let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
            self.after_source_resolved(path);
            if !metadata.is_file() {
                return Err(invalid("registered source must be a file"));
            }
            let stamp = SourceStamp::of(&metadata);
            if !build.seen.contains(&stamp.identity) {
                budget.reserve(set_growth(&build.seen, 1) + vec_growth(&build.members, 1))?;
                budget.retain_scratch(canonical.capacity());
                build.seen.insert(stamp.identity);
                build.members.push(PreparedSourceRef {
                    path: canonical,
                    stamp,
                });
            }
        }
        build.source_next += 1;
        #[cfg(test)]
        self.membership_checkpoint("source", build.source_next)?;
        Ok(())
    }

    fn membership_sidechain_step(
        &self,
        build: &mut MembershipBuild,
        listing_validated: &mut bool,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
        budget: &mut CatalogBudget<'_, '_>,
    ) -> Result<(), SnapshotError> {
        if build.examined == build.members.len() {
            build.phase = MembershipPhase::Finish;
            return Ok(());
        }
        if build.listing.is_none() {
            budget.scratch(3 * FILESYSTEM_PATH_BYTES)?;
            let source = &build.members[build.examined];
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
                    budget.reserve(vec_growth(&build.sidechain_dirs, 1))?;
                    budget.retain_scratch(directory.capacity());
                    build.sidechain_dirs.push((directory, None));
                    build.examined += 1;
                    return Ok(());
                }
                Err(error) => return Err(io_error(error)),
            };
            self.authority(budget.context, Some(&canonical))?;
            let stamp = SourceStamp::of(&std::fs::metadata(&canonical).map_err(io_error)?);
            budget.reserve(
                vec_growth(&build.sidechain_dirs, 1) + READ_DIR_HANDLE_BYTES + canonical.capacity(),
            )?;
            let entries = OpenDirectory::open(&canonical)?;
            if entries.stamp() != stamp {
                return Err(SnapshotError::new(
                    Status::Changed,
                    "registered sidechain changed before opening",
                ));
            }
            budget.retain_scratch(canonical.capacity());
            build.sidechain_dirs.push((canonical, Some(stamp)));
            build.listing = Some(MembershipListing::Reading {
                entries,
                children: Vec::new(),
                pending: None,
            });
            *listing_validated = true;
        }
        match build
            .listing
            .as_mut()
            .expect("membership sidechain listing")
        {
            MembershipListing::Reading {
                entries,
                children,
                pending,
            } => {
                if !*listing_validated {
                    let (path, stamp) = build
                        .sidechain_dirs
                        .last()
                        .expect("membership sidechain directory");
                    self.authority(budget.context, Some(path))?;
                    let current = SourceStamp::of(&std::fs::metadata(path).map_err(io_error)?);
                    if Some(current) != *stamp
                        || entries.entries.current_stamp().map_err(io_error)? != current
                    {
                        return Err(SnapshotError::new(
                            Status::Changed,
                            "registered sidechain changed between steps",
                        ));
                    }
                    *listing_validated = true;
                }
                budget.scratch(3 * FILESYSTEM_PATH_BYTES)?;
                if pending.is_none() {
                    match entries.next() {
                        Some(entry) => *pending = Some(entry.map_err(io_error)?),
                        None => {
                            children.sort();
                            let children = std::mem::take(children);
                            build.listing = Some(MembershipListing::Selected { children, next: 0 });
                            return Ok(());
                        }
                    }
                }
                if remaining.max_discovery_entries == 0 {
                    budget.retain_scratch(
                        pending
                            .as_ref()
                            .expect("pending sidechain entry")
                            .name
                            .capacity(),
                    );
                    return Err(SnapshotError::new(
                        Status::Incomplete,
                        "registered sidechain discovery budget exhausted",
                    ));
                }
                remaining.max_discovery_entries -= 1;
                usage[17] += 1;
                let entry = pending.take().expect("pending sidechain entry");
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && !entry.name.to_string_lossy().starts_with("._")
                {
                    budget.reserve(vec_growth(children, 1))?;
                    budget.retain_scratch(path.capacity());
                    children.push(path);
                }
                #[cfg(test)]
                self.membership_checkpoint("sidechain_entry", children.len())?;
            }
            MembershipListing::Selected { children, next } => {
                if *next == children.len() {
                    build.listing = None;
                    build.examined += 1;
                    *listing_validated = false;
                    return Ok(());
                }
                budget.scratch(3 * FILESYSTEM_PATH_BYTES)?;
                let canonical = std::fs::canonicalize(&children[*next]).map_err(io_error)?;
                self.authority(budget.context, Some(&canonical))?;
                let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
                if !metadata.is_file() {
                    return Err(invalid("registered sidechain must be a file"));
                }
                let stamp = SourceStamp::of(&metadata);
                if !build.seen.contains(&stamp.identity) {
                    budget.reserve(set_growth(&build.seen, 1) + vec_growth(&build.members, 1))?;
                    budget.retain_scratch(canonical.capacity());
                    build.seen.insert(stamp.identity);
                    build.members.push(PreparedSourceRef {
                        path: canonical,
                        stamp,
                    });
                }
                *next += 1;
            }
        }
        Ok(())
    }
}

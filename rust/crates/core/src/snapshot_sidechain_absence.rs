struct SidechainBatchRow<'a> {
    parent: &'a Path,
    name: &'a OsStr,
    index: usize,
}

struct SidechainBatchParent<'a> {
    parent: &'a Path,
    rows: Range<usize>,
    stamp: Option<SourceStamp>,
}

struct SidechainAbsences<'a> {
    bits: Vec<u64>,
    _reservation: ProjectionReservation<'a>,
}

impl SidechainAbsences<'_> {
    fn contains(&self, index: usize) -> bool {
        self.bits[index / 64] & (1 << (index % 64)) != 0
    }
}

struct SidechainCatalogClaim<'a> {
    store: &'a NativeStore,
    slot: Arc<CatalogSlot>,
    catalog: Option<SourceCatalog>,
}

impl Drop for SidechainCatalogClaim<'_> {
    fn drop(&mut self) {
        let _state = self.store.lock_state();
        *self.slot.cell.lock().expect("source catalog") =
            CatalogCell::Idle(self.catalog.take().expect("sidechain catalog claim"));
        self.store.fact_read_released.notify_all();
    }
}

fn sidechain_batch_prefix(path: &Path) -> Option<(&Path, &OsStr)> {
    if path.file_name() != Some(OsStr::new("subagents")) { return None; }
    let prefix = path.parent()?;
    Some((prefix.parent()?, prefix.file_name()?))
}

impl NativeStore {
    fn sidechain_absences<'a>(
        &'a self,
        certificate: Option<CatalogCertificate>,
        paths: &[(PathBuf, Option<SourceStamp>)],
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<Option<SidechainAbsences<'a>>, SnapshotError> {
        let Some(certificate) = certificate else { return Ok(None); };
        let mut count = 0;
        let mut longest = 0;
        for (path, stamp) in paths {
            cancel.check(deadline)?;
            if stamp.is_none() {
                if let Some((parent, _)) = sidechain_batch_prefix(path) {
                    count += 1;
                    longest = longest.max(parent.as_os_str().len());
                }
            }
        }
        if count == 0 { return Ok(None); }
        let words = paths.len().div_ceil(64);
        let bit_bytes = words * size_of::<u64>();
        let proof_scratch = READ_DIR_HANDLE_BYTES + longest + FILESYSTEM_PATH_BYTES;
        let bytes = count * size_of::<SidechainBatchRow>() + bit_bytes + proof_scratch;
        if !self.admit_spare(context, bytes) { return Ok(None); }
        let mut reservation = ProjectionReservation { store: self, bytes };
        let claim = {
            let state = self.lock_state();
            let Some(slot) = state.catalogs.get(certificate.key()) else { return Ok(None); };
            let slot = Arc::clone(slot);
            let mut cell = slot.cell.lock().expect("source catalog");
            if !matches!(&*cell, CatalogCell::Idle(catalog) if certificate.matches(catalog) && catalog.context["authority"] == context["authority"]) {
                return Ok(None);
            }
            let CatalogCell::Idle(catalog) = std::mem::replace(&mut *cell, CatalogCell::Busy) else {
                unreachable!("matching sidechain catalog");
            };
            drop(cell);
            SidechainCatalogClaim { store: self, slot, catalog: Some(catalog) }
        };
        let mut bits = vec![0u64; words];
        {
            let mut rows = Vec::with_capacity(count);
            for (index, (path, stamp)) in paths.iter().enumerate() {
                cancel.check(deadline)?;
                if stamp.is_none() {
                    if let Some((parent, name)) = sidechain_batch_prefix(path) {
                        rows.push(SidechainBatchRow { parent, name, index });
                    }
                }
            }
            rows.sort_unstable_by(|left, right|
                left.parent.cmp(right.parent).then_with(|| left.name.cmp(right.name)));
            let parent_count = 1 + rows.windows(2).filter(|pair| pair[0].parent != pair[1].parent).count();
            let parent_bytes = parent_count * size_of::<SidechainBatchParent>();
            if !self.admit_spare(context, parent_bytes) { return Ok(None); }
            reservation.bytes += parent_bytes;
            let mut parents: Vec<SidechainBatchParent> = Vec::with_capacity(parent_count);
            for (index, row) in rows.iter().enumerate() {
                cancel.check(deadline)?;
                if let Some(parent) = parents.last_mut().filter(|parent| parent.parent == row.parent) {
                    parent.rows.end = index + 1;
                } else {
                    parents.push(SidechainBatchParent { parent: row.parent, rows: index..index + 1, stamp: None });
                }
            }
            let catalog = claim.catalog.as_ref().expect("sidechain catalog");
            for (identity, node) in catalog.directories.iter() {
                cancel.check(deadline)?;
                let Ok(index) = parents.binary_search_by(|parent| parent.parent.cmp(node.canonical.as_path())) else { continue; };
                if !matches!(node.scan, CatalogScan::Done) || node.pending_name.is_some()
                    || !node.conditional.is_empty() || !node.issues.is_empty()
                    || !catalog.route_index.contains_key(identity)
                    || catalog.pending_edge.as_ref().is_some_and(|edge| edge.directory == *identity)
                    || catalog.pending_issue.as_ref().is_some_and(|issue| issue.directory == *identity)
                { continue; }
                let parent = &mut parents[index];
                parent.stamp = Some(node.observed.stamp.expect("closed sidechain parent stamp"));
                let requested = &rows[parent.rows.clone()];
                for row in requested { bits[row.index / 64] |= 1 << (row.index % 64); }
                for edge in &node.edges {
                    cancel.check(deadline)?;
                    #[cfg(test)]
                    self.sidechain_batch_name_checks.fetch_add(1, Ordering::Relaxed);
                    #[cfg(test)]
                    self.after_location_metadata("sidechain_batch_name");
                    let start = requested.partition_point(|row| row.name < edge.name.as_os_str());
                    for row in &requested[start..] {
                        if row.name != edge.name.as_os_str() { break; }
                        bits[row.index / 64] &= !(1 << (row.index % 64));
                    }
                }
            }
            drop(claim);
            let mut budget = CatalogBudget { store: self, context, reservation: &mut reservation,
                attempt: 0, scratch_bytes: proof_scratch };
            for parent in &parents {
                cancel.check(deadline)?;
                let requested = &rows[parent.rows.clone()];
                if !requested.iter().any(|row| bits[row.index / 64] & (1 << (row.index % 64)) != 0) {
                    continue;
                }
                let Some(stamp) = parent.stamp else { continue; };
                let request = CatalogProofRequest { store: self, context, cancel, deadline_unix_ms: deadline };
                let current = match catalog_open_proof_directory(&request, parent.parent, stamp, &mut budget) {
                    Ok(CatalogProofDirectory::Opened(directory)) => { drop(directory); true }
                    Ok(_) => false,
                    Err(error) if matches!(error.status, Status::Deadline | Status::Cancelled) => return Err(error),
                    Err(_) => false,
                };
                if !current {
                    for row in requested { bits[row.index / 64] &= !(1 << (row.index % 64)); }
                }
            }
        }
        let scratch_bytes = reservation.bytes - bit_bytes;
        reservation.bytes = bit_bytes;
        drop(ProjectionReservation { store: self, bytes: scratch_bytes });
        if bits.iter().all(|word| *word == 0) { return Ok(None); }
        Ok(Some(SidechainAbsences { bits, _reservation: reservation }))
    }
}

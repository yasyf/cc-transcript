#[derive(Clone, Copy)]
enum RawProjectionKind {
    Excerpt,
    Tail(usize),
}

struct ReservedRawKey<'a> {
    key: Vec<u8>,
    reservation: ProjectionReservation<'a>,
}

impl NativeStore {
    fn raw_projection_source(
        &self, handle: &Value, raw: &RawSourceScope, context: &Value,
        cancel: &Cancellation, bounds: WorkLimits,
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        cancel.check(bounds.deadline_unix_ms)?;
        let source = self.capture_scope(handle, context)?.snapshot;
        let current_path = std::fs::canonicalize(&raw.snapshot.canonical_path).map_err(io_error)?;
        self.authority(context, Some(&current_path))?;
        if !Arc::ptr_eq(&source, &raw.source)
            || raw.snapshot.stamp.identity != source.stamp.identity.file()
            || raw.snapshot.provider != source.provider
            || raw.snapshot.session_id != source.session_id
        {
            return Err(SnapshotError::new(Status::StaleHandle, "raw scope belongs to another source generation"));
        }
        if raw.snapshot.provider != Provider::Claude {
            return Err(invalid("raw projection requires a Claude source"));
        }
        let state = self.lock_state();
        let registry = str_field(context, "registry_generation")?;
        let paired = state.generations.get(&snapshot_key(&raw.snapshot)).is_some_and(|record| {
            record.registry_generation == registry
                && std::ptr::eq(record.snapshot.as_ptr(), Arc::as_ptr(&raw.snapshot))
                && record.raw_window.as_ref().is_some_and(|window| Arc::ptr_eq(window, &raw.window))
        });
        if !paired {
            return Err(SnapshotError::new(Status::StaleHandle, "raw scope generation is not registered for this context"));
        }
        cancel.check(bounds.deadline_unix_ms)?;
        Ok(source)
    }

    fn raw_materialize_scope<'a, I: Iterator<Item = &'a [u8]>>(
        &self, handle: &Value, source: Arc<TranscriptSnapshot>, raw: &'a RawSourceScope,
        kind: RawProjectionKind, mut work: ReservedRawKey<'_>,
        selection_bytes: usize, make_lines: impl FnOnce() -> I,
        context: &Value, cancel: &Cancellation, bounds: WorkLimits, usage: &mut [u64; 20],
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        let cached = {
            let state = self.lock_state();
            self.lease(&state, handle, context)?;
            state.generations.get(&snapshot_key(&source))
                .and_then(|record| record.raw_cache.as_ref())
                .filter(|cache| Arc::ptr_eq(&cache.raw, &raw.snapshot))
                .and_then(|cache| match kind {
                    RawProjectionKind::Excerpt => cache.excerpt.as_ref(),
                    RawProjectionKind::Tail(_) => cache.tail.as_ref(),
                })
                .filter(|scope| scope.key == work.key)
                .map(|scope| Arc::clone(&scope.snapshot))
        };
        if let Some(snapshot) = cached {
            self.raw_projection_source(handle, raw, context, cancel, bounds)?;
            return Ok(snapshot);
        }
        if selection_bytes > 0 {
            self.extend_projection_reservation(&mut work.reservation, context, selection_bytes)?;
        }
        let working = work.reservation.bytes;
        let mut decoded = 0usize;
        let mut entries = Vec::new();
        let mut arenas = Vec::new();
        let chunks = Vec::new();
        let mut lines = make_lines();
        while match kind {
            RawProjectionKind::Excerpt => true,
            RawProjectionKind::Tail(count) => entries.len() < count,
        } {
            let Some(line) = lines.next() else { break };
            cancel.check(bounds.deadline_unix_ms)?;
            if usage[2].saturating_add(line.len() as u64) > bounds.max_read_bytes as u64 {
                return Err(SnapshotError::new(Status::SourceLimit, "raw projection read budget exhausted"));
            }
            if line.len() > self.config.entry {
                return Err(SnapshotError::new(Status::EntryLimit, "source entry exceeds owner bound"));
            }
            let dom_bound = dom_parse_bound(line.len())
                .ok_or_else(|| SnapshotError::new(Status::RetainedLimit, "line bound overflows"))?;
            let needed = working.saturating_add(decoded).saturating_add(dom_bound);
            let growth = needed.saturating_sub(work.reservation.bytes);
            if growth > 0 {
                self.extend_projection_reservation(&mut work.reservation, context, growth)?;
            }
            self.before_decode("dom");
            let Some(dom) = crate::parse::parse_line_dom(line) else {
                usage[2] += line.len() as u64;
                usage[3] += 1;
                continue;
            };
            let typed_bound = Self::decoded_line_bound(&entries, &arenas, &chunks, false, &dom, line.len(), 0)
                .ok_or_else(|| SnapshotError::new(Status::RetainedLimit, "line bound overflows"))?;
            let needed = needed.saturating_add(typed_bound);
            let growth = needed.saturating_sub(work.reservation.bytes);
            if growth > 0 {
                self.extend_projection_reservation(&mut work.reservation, context, growth)?;
            }
            self.before_decode("entry");
            let root = dom.clone();
            let mut retained = crate::parse::Retained::default();
            let entry = crate::parse::entry_from_dom(dom, &mut retained, &|_| true)
                .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
            let fresh = if entry.is_some() { retained.arenas(root, line.len()) } else { Vec::new() };
            let charge = Self::decoded_line_bytes(&entries, entry.as_slice(), &arenas, &fresh, &chunks, false);
            assert!(typed_bound >= charge, "raw decoded line exceeded its admitted bound");
            decoded += charge;
            if let Some(entry) = entry {
                if entries.len() >= bounds.max_events {
                    return Err(SnapshotError::new(Status::EntryLimit, "raw projection event budget exhausted"));
                }
                let position = entries.len();
                arenas.extend(fresh.into_iter().map(|arena| (position, arena)));
                entries.push(entry);
            }
            usage[2] += line.len() as u64;
            usage[3] += 1;
        }
        drop(lines);
        if let RawProjectionKind::Tail(count) = kind {
            if entries.len() < count && raw.snapshot.window_start > 0 {
                return Err(SnapshotError::new(Status::SourceLimit, "raw tail does not cover the requested complete events"));
            }
            entries.reverse();
            let count = entries.len();
            for (position, _) in &mut arenas {
                *position = count - 1 - *position;
            }
        }
        let registry = str_field(context, "registry_generation")?;
        let session_id = match entries.iter().find_map(Entry::meta) {
            Some(meta) => Cow::Borrowed(meta.session_id.as_str()),
            None => {
                let stem = raw.snapshot.canonical_path.file_stem().expect("source filename");
                let fallback_capacity_bound = crate::parse::pushed_capacity(stem.as_encoded_bytes().len() * 3) * 2;
                let needed = working.saturating_add(decoded).saturating_add(fallback_capacity_bound);
                let growth = needed.saturating_sub(work.reservation.bytes);
                if growth > 0 {
                    self.extend_projection_reservation(&mut work.reservation, context, growth)?;
                }
                stem.to_string_lossy()
            }
        };
        let session_bytes = match &session_id {
            Cow::Borrowed(value) => value.len(),
            Cow::Owned(value) => value.capacity(),
        };
        let bytes = entries.iter().map(entry_charge).fold(0usize, |total, charge| {
            total.saturating_add(charge.owned_capacity_bytes).saturating_add(charge.opaque_dom_accounted_bytes)
        });
        let calls = entries.iter().map(|entry| entry.tool_uses().count()).sum();
        let results = entries.iter().map(|entry| entry.tool_results().count()).sum();
        let extension = bytes.saturating_mul(2)
            .saturating_add(entries.len() * size_of::<&Entry>())
            .saturating_add(arc_bytes::<EntryChunk>() + arc_bytes::<ChunkRows>() + size_of::<Arc<EntryChunk>>())
            .saturating_add(arc_bytes::<ActivityIndex>() + ActivityIndex::EMPTY_ALLOCATION_BYTES)
            .saturating_add(raw.snapshot.activity.append_container_reservation_bytes(entries.len(), calls, results))
            .saturating_add(arc_bytes::<TranscriptSnapshot>() + 64 + raw.snapshot.canonical_path.as_os_str().len() + session_bytes + registry.len())
            .saturating_add(3 * size_of::<(usize, MemoryCharge)>() + crate::parse::pushed_capacity(4 + entries.len() + calls) * size_of::<(usize, usize)>());
        let needed = working.saturating_add(decoded).saturating_add(extension);
        let growth = needed.saturating_sub(work.reservation.bytes);
        if growth > 0 {
            self.extend_projection_reservation(&mut work.reservation, context, growth)?;
        }
        let references: Vec<_> = entries.iter().collect();
        let activity = Arc::new(ActivityIndex::new(&references, None));
        drop(references);
        let event_count = entries.len();
        let session_id = session_id.into_owned();
        let snapshot = Arc::new(TranscriptSnapshot {
            ledger: LedgerHook::default(), id: self.token(match kind { RawProjectionKind::Excerpt => "raw-excerpt", RawProjectionKind::Tail(_) => "raw-tail" }),
            canonical_path: raw.snapshot.canonical_path.clone(), stamp: raw.snapshot.stamp,
            provider: Provider::Claude, session_id,
            chunks: vec![Arc::new(EntryChunk::retaining(0, entries, arenas))], activity,
            window_start: raw.snapshot.window_start, committed_bytes: raw.snapshot.committed_bytes,
            provisional_tail: false, fence: Vec::new(), event_count, codex_raw: None, codex_append: None,
        });
        let mut generation = GenerationRecord::new(&snapshot, registry);
        generation.raw_parent = Some(Arc::clone(&raw.snapshot));
        self.raw_projection_source(handle, raw, context, cancel, bounds)?;
        let mut state = self.lock_state();
        self.lease(&state, handle, context)?;
        let snapshot_id = snapshot_key(&snapshot);
        let additional = state.admission(&snapshot_id, &generation, generation.anchors())
            + state.generations.growth_for(&snapshot_id) + work.key.capacity();
        self.admit_memory(&mut state, context, additional.saturating_sub(work.reservation.bytes))?;
        cancel.check(bounds.deadline_unix_ms)?;
        state.register_generation(&snapshot, generation);
        {
            let mut parent = state.generations.get_mut(&snapshot_key(&source)).expect("leased source generation");
            if let Some(cache) = parent.raw_cache.as_mut().filter(|cache| Arc::ptr_eq(&cache.raw, &raw.snapshot)) {
                let cached = Some(CachedRawScope { key: work.key, snapshot: Arc::clone(&snapshot) });
                match kind {
                    RawProjectionKind::Excerpt => cache.excerpt = cached,
                    RawProjectionKind::Tail(_) => cache.tail = cached,
                }
            } else {
                drop(work.key);
            }
        }
        state.transient_bytes -= work.reservation.bytes;
        work.reservation.bytes = 0;
        Ok(snapshot)
    }
}

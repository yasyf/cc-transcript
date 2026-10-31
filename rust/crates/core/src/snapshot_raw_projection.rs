impl NativeStore {
    fn raw_excerpt_scope(
        &self, handle: &Value, forms: &[&[u8]], around: usize, limit: usize,
        context: &Value, cancel: &Cancellation, bounds: WorkLimits, usage: &mut [u64; 20],
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        let raw = self.raw_source_scope(handle, 16 * 1024 * 1024, context, cancel, bounds, usage)?;
        if raw.snapshot.provider != Provider::Claude {
            return Err(invalid("raw excerpt projection requires a Claude source"));
        }
        let source = self.pin_record(handle, context)?.snapshot;
        let key_bound = forms.iter().map(|form| form.len() + size_of::<u64>()).sum::<usize>()
            + 2 * size_of::<u64>();
        let mut reservation = self.reserve_projection(context, key_bound)?;
        let mut key = Vec::with_capacity(key_bound);
        key.extend_from_slice(&(around as u64).to_le_bytes());
        key.extend_from_slice(&(limit as u64).to_le_bytes());
        for form in forms {
            key.extend_from_slice(&(form.len() as u64).to_le_bytes());
            key.extend_from_slice(form);
        }
        {
            let state = self.lock_state();
            if let Some(cached) = state.generations.get(&snapshot_key(&source))
                .and_then(|record| record.raw_cache.as_ref())
                .filter(|cache| Arc::ptr_eq(&cache.raw, &raw.snapshot))
                .and_then(|cache| cache.excerpt.as_ref())
                .filter(|scope| scope.key == key)
            {
                return Ok(Arc::clone(&cached.snapshot));
            }
        }
        self.extend_projection_reservation(&mut reservation, context, raw.window.selection_work_bound(limit))?;
        let ranges = raw.window.select(forms, around, limit);
        let working = reservation.bytes;
        let mut decoded = 0usize;
        let mut entries = Vec::new();
        let mut arenas = Vec::new();
        let chunks = Vec::new();
        for range in ranges {
            for line in raw.window.selected_bytes(range).split_inclusive(|byte| *byte == b'\n') {
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
                let growth = needed.saturating_sub(reservation.bytes);
                self.extend_projection_reservation(&mut reservation, context, growth)?;
                self.before_decode("dom");
                let Some(dom) = crate::parse::parse_line_dom(line) else {
                    usage[2] += line.len() as u64;
                    usage[3] += 1;
                    continue;
                };
                let typed_bound = Self::decoded_line_bound(&entries, &arenas, &chunks, false, &dom, line.len(), 0)
                    .ok_or_else(|| SnapshotError::new(Status::RetainedLimit, "line bound overflows"))?;
                let needed = needed.saturating_add(typed_bound);
                let growth = needed.saturating_sub(reservation.bytes);
                self.extend_projection_reservation(&mut reservation, context, growth)?;
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
        }
        let registry = str_field(context, "registry_generation")?;
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
            .saturating_add(arc_bytes::<TranscriptSnapshot>() + 64 + raw.snapshot.canonical_path.as_os_str().len() + raw.snapshot.session_id.len() + registry.len())
            .saturating_add(3 * size_of::<(usize, MemoryCharge)>() + crate::parse::pushed_capacity(4 + entries.len() + calls) * size_of::<(usize, usize)>());
        let needed = working.saturating_add(decoded).saturating_add(extension);
        let growth = needed.saturating_sub(reservation.bytes);
        self.extend_projection_reservation(&mut reservation, context, growth)?;
        let references: Vec<_> = entries.iter().collect();
        let activity = Arc::new(ActivityIndex::new(&references, None));
        drop(references);
        let event_count = entries.len();
        let snapshot = Arc::new(TranscriptSnapshot {
            ledger: LedgerHook::default(), id: self.token("raw-excerpt"),
            canonical_path: raw.snapshot.canonical_path.clone(), stamp: raw.snapshot.stamp,
            provider: Provider::Claude, session_id: raw.snapshot.session_id.clone(),
            chunks: vec![Arc::new(EntryChunk::retaining(0, entries, arenas))], activity,
            window_start: raw.snapshot.window_start, committed_bytes: raw.snapshot.committed_bytes,
            provisional_tail: false, fence: Vec::new(), event_count, codex_raw: None, codex_append: None,
        });
        let mut generation = GenerationRecord::new(&snapshot, registry);
        generation.raw_parent = Some(Arc::clone(&raw.snapshot));
        let mut state = self.lock_state();
        self.lease(&state, handle, context)?;
        let snapshot_id = snapshot_key(&snapshot);
        let additional = state.admission(&snapshot_id, &generation, generation.anchors())
            + state.generations.growth_for(&snapshot_id) + key.capacity();
        self.admit_memory(&mut state, context, additional.saturating_sub(reservation.bytes))?;
        state.transient_bytes -= reservation.bytes;
        reservation.bytes = 0;
        state.register_generation(&snapshot, generation);
        let mut parent = state.generations.get_mut(&snapshot_key(&source)).expect("leased source generation");
        if let Some(cache) = parent.raw_cache.as_mut().filter(|cache| Arc::ptr_eq(&cache.raw, &raw.snapshot)) {
            cache.excerpt = Some(CachedRawScope { key, snapshot: Arc::clone(&snapshot) });
        }
        Ok(snapshot)
    }
}

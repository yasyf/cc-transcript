struct CachedRawScope {
    key: Vec<u8>,
    snapshot: Arc<TranscriptSnapshot>,
}

struct RawCapabilityCache {
    stamp: SourceStamp,
    tail_bytes: u64,
    raw: Arc<TranscriptSnapshot>,
    excerpt: Option<CachedRawScope>,
    tail: Option<CachedRawScope>,
}

impl RawCapabilityCache {
    fn metadata_bytes(&self) -> usize {
        size_of::<Self>()
            + self.excerpt.as_ref().map_or(0, |scope| scope.key.capacity())
            + self.tail.as_ref().map_or(0, |scope| scope.key.capacity())
    }
}

fn raw_window_anchors(window: &Arc<crate::snapshot_raw_window::RawWindow>) -> impl Iterator<Item = Anchor> {
    std::iter::once(Anchor::entries(
        Arc::as_ptr(window) as usize,
        MemoryCharge { owned_capacity_bytes: arc_bytes::<crate::snapshot_raw_window::RawWindow>(), opaque_dom_accounted_bytes: 0 },
    )).chain(window.payload_allocation().map(|(id, bytes)| Anchor::entries(
        id, MemoryCharge { owned_capacity_bytes: bytes, opaque_dom_accounted_bytes: 0 },
    )))
}

struct RawSourceScope {
    window: Arc<crate::snapshot_raw_window::RawWindow>,
    snapshot: Arc<TranscriptSnapshot>,
    source: Arc<TranscriptSnapshot>,
}

impl NativeStore {
    fn raw_source_scope(
        &self,
        handle: &Value,
        tail_bytes: u64,
        context: &Value,
        cancel: &Cancellation,
        bounds: WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<RawSourceScope, SnapshotError> {
        use std::os::unix::fs::OpenOptionsExt;
        if tail_bytes == 0 || tail_bytes > 16 * 1024 * 1024 {
            return Err(invalid("raw source tail must be within 16 MiB"));
        }
        let source = self.pin_record(handle, context)?.snapshot;
        let _claim = self.claim_fact_read(cancel, bounds.deadline_unix_ms)?;
        cancel.check(bounds.deadline_unix_ms)?;
        let path = std::fs::canonicalize(&source.canonical_path).map_err(io_error)?;
        self.authority(context, Some(&path))?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .map_err(io_error)?;
        usage[0] += 1;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("source must be a regular file"));
        }
        let stamp = SourceStamp::of(&metadata);
        if stamp.identity != source.stamp.identity.file() {
            return Err(SnapshotError::new(Status::Changed, "source identity changed"));
        }
        let key = snapshot_key(&source);
        let cached = {
            let state = self.lock_state();
            state.generations.get(&key)
                .and_then(|record| record.raw_cache.as_ref())
                .filter(|cache| cache.stamp == stamp && cache.tail_bytes == tail_bytes)
                .map(|cache| Arc::clone(&cache.raw))
        };
        if let Some(snapshot) = cached {
            self.validate_raw_source(&file, &path, stamp, context, cancel, bounds.deadline_unix_ms)?;
            self.lease(&self.lock_state(), handle, context)?;
            let window = Arc::clone(self.lock_state().generations
                .get(&snapshot_key(&snapshot)).expect("live raw generation")
                .raw_window.as_ref().expect("raw generation payload"));
            return Ok(RawSourceScope { source, snapshot, window });
        }
        let start = stamp.size.saturating_sub(tail_bytes);
        let read_start = start.saturating_sub(u64::from(start > 0));
        let required = usize::try_from(stamp.size - read_start)
            .map_err(|_| invalid("raw source size does not fit address space"))?;
        if required > bounds.max_source_read_bytes.min(self.config.source) {
            return Err(SnapshotError::new(Status::SourceLimit, "raw source read budget exhausted"));
        }
        let registry = str_field(context, "registry_generation")?;
        let metadata_bound = arc_bytes::<TranscriptSnapshot>()
            + arc_bytes::<ActivityIndex>() + ActivityIndex::EMPTY_ALLOCATION_BYTES
            + arc_bytes::<crate::snapshot_raw_window::RawWindow>()
            + size_of::<RawCapabilityCache>() + 64
            + path.capacity() + source.session_id.len() + registry.len()
            + size_of::<(usize, MemoryCharge)>()
            + 4 * size_of::<(usize, usize)>();
        let mut reservation = self.reserve_projection(context, required + metadata_bound)?;
        file.seek(SeekFrom::Start(read_start)).map_err(io_error)?;
        let mut prefix = [0u8; 8192];
        let mut position = read_start;
        let mut prefix_used = 0;
        let mut prefix_start = 0;
        if start > 0 {
            loop {
                cancel.check(bounds.deadline_unix_ms)?;
                let count = prefix.len().min((stamp.size - position) as usize);
                if count == 0 {
                    break;
                }
                self.before_source_read();
                file.read_exact(&mut prefix[..count]).map_err(io_error)?;
                usage[1] += count as u64;
                position += count as u64;
                if let Some(newline) = memchr::memchr(b'\n', &prefix[..count]) {
                    prefix_start = newline + 1;
                    prefix_used = count;
                    break;
                }
            }
        }
        let window_start = position - (prefix_used - prefix_start) as u64;
        let length = (stamp.size - window_start) as usize;
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&prefix[prefix_start..prefix_used]);
        bytes.resize(length, 0);
        let mut filled = prefix_used - prefix_start;
        while filled < length {
            cancel.check(bounds.deadline_unix_ms)?;
            let end = length.min(filled + 64 * 1024);
            self.before_source_read();
            file.read_exact(&mut bytes[filled..end]).map_err(io_error)?;
            usage[1] += (end - filled) as u64;
            filled = end;
        }
        self.validate_raw_source(&file, &path, stamp, context, cancel, bounds.deadline_unix_ms)?;
        let window = Arc::new(crate::snapshot_raw_window::RawWindow::from_line_boundary_bytes(bytes));
        let snapshot = Arc::new(TranscriptSnapshot {
            ledger: LedgerHook::default(), id: self.token("raw-generation"),
            canonical_path: path, stamp, provider: source.provider,
            session_id: source.session_id.clone(), chunks: Vec::new(),
            activity: Arc::new(ActivityIndex::default()), window_start,
            committed_bytes: stamp.size, provisional_tail: false,
            fence: Vec::new(), event_count: 0, codex_raw: None, codex_append: None,
        });
        let mut generation = GenerationRecord::new(&snapshot, registry);
        generation.raw_window = Some(Arc::clone(&window));
        let cache = Box::new(RawCapabilityCache {
            stamp, tail_bytes, raw: Arc::clone(&snapshot), excerpt: None, tail: None,
        });
        let mut state = self.lock_state();
        self.lease(&state, handle, context)?;
        let raw_key = snapshot_key(&snapshot);
        let additional = state.admission(&raw_key, &generation, generation.anchors())
            + state.generations.growth_for(&raw_key) + cache.metadata_bytes();
        self.admit_memory(&mut state, context, additional.saturating_sub(reservation.bytes))?;
        state.transient_bytes -= reservation.bytes;
        reservation.bytes = 0;
        state.register_generation(&snapshot, generation);
        state.generations.get_mut(&key).expect("leased source generation").raw_cache = Some(cache);
        Ok(RawSourceScope { source, snapshot, window })
    }

    fn validate_raw_source(
        &self, file: &File, path: &Path, stamp: SourceStamp, context: &Value,
        cancel: &Cancellation, deadline: u64,
    ) -> Result<(), SnapshotError> {
        cancel.check(deadline)?;
        let current = std::fs::canonicalize(path).map_err(io_error)?;
        self.authority(context, Some(&current))?;
        if SourceStamp::of(&file.metadata().map_err(io_error)?) != stamp
            || SourceStamp::of(&std::fs::metadata(path).map_err(io_error)?) != stamp
        {
            return Err(SnapshotError::new(Status::Changed, "source changed while reading raw evidence"));
        }
        Ok(())
    }
}
